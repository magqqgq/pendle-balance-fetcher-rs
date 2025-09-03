use alloy::{
    primitives::{Address, U256},
    providers::Provider,
};
use anyhow::Result;

use crate::{
    RpcProvider,
    constants::{MULTICALL_ADDRESS, MULTICALL_BATCH_SIZE},
    contracts::{
        Multicall::MulticallInstance, Multicall3::Call, PendleMarket::PendleMarketInstance,
        PendleYieldToken::PendleYieldTokenInstance,
    },
    types::YtInterestData,
};

#[derive(Debug, Clone)]
pub struct Multicall {
    instance: MulticallInstance<RpcProvider>,
    provider: RpcProvider,
}

#[derive(Debug, Clone)]
pub struct YtGeneralData {
    pub is_expired: bool,
    pub sy_reserve: U256,
    pub factory: Address,
}

impl Multicall {
    pub fn new(provider: RpcProvider) -> Self {
        Self {
            instance: MulticallInstance::new(*MULTICALL_ADDRESS, provider.clone()),
            provider,
        }
    }

    /// Fetches ERC20 balances for multiple token-address pairs using Multicall
    /// Each token[i] is paired with address[i]
    pub async fn get_all_erc20_balances_multi_tokens(
        &self,
        tokens: &[Address],
        addresses: &[Address],
        block_number: u64,
    ) -> Result<Vec<U256>> {
        if tokens.len() != addresses.len() {
            return Err(anyhow::anyhow!(
                "Tokens and addresses arrays must have the same length"
            ));
        }

        let calls: Vec<Call> = tokens
            .iter()
            .zip(addresses.iter())
            .map(|(&token, &address)| {
                let token_instance = PendleMarketInstance::new(token, self.provider.clone());
                Call {
                    target: token,
                    callData: token_instance.balanceOf(address).calldata().to_vec().into(),
                }
            })
            .collect();

        let result = self
            .instance
            .aggregate(calls)
            .call()
            .block(block_number.into())
            .await?;

        // Decode the return data
        let balances: Vec<U256> = result
            .returnData
            .into_iter()
            .map(|data| {
                if data.is_empty() {
                    U256::ZERO
                } else {
                    U256::from_be_bytes::<32>(data.as_ref().try_into().unwrap_or([0u8; 32]))
                }
            })
            .collect();

        Ok(balances)
    }

    pub async fn try_aggregate_multicall(
        &self,
        calls: Vec<Call>,
        block_number: u64,
    ) -> Result<Vec<Vec<u8>>> {
        let mut all_results = Vec::new();

        // Process in batches
        for chunk in calls.chunks(MULTICALL_BATCH_SIZE) {
            let result = self
                .instance
                .tryAggregate(false, chunk.to_vec())
                .call()
                .block(block_number.into())
                .await?;

            // Extract successful results
            for result_item in result.iter() {
                if result_item.success {
                    all_results.push(result_item.returnData.to_vec());
                } else {
                    all_results.push(vec![]);
                }
            }
        }

        Ok(all_results)
    }

    /// Fetches ERC20 balances for multiple addresses using the Multicall Builder.
    pub async fn get_all_erc20_balances(
        &self,
        token_address: Address,
        addresses: &[Address],
        block_number: u64,
    ) -> Result<Vec<U256>> {
        let pendle_market_instance =
            PendleMarketInstance::new(token_address, self.provider.clone());

        let calls: Vec<Call> = addresses
            .iter()
            .map(|&address| Call {
                target: token_address,
                callData: pendle_market_instance
                    .balanceOf(address)
                    .calldata()
                    .to_vec()
                    .into(),
            })
            .collect();

        let results = self.try_aggregate_multicall(calls, block_number).await?;

        // Decode balances, defaulting to 0 for failed calls
        let balances: Vec<U256> = results
            .into_iter()
            .map(|data| {
                if data.is_empty() {
                    U256::ZERO
                } else {
                    U256::from_be_bytes::<32>(data.try_into().unwrap_or([0u8; 32]))
                }
            })
            .collect();

        Ok(balances)
    }

    pub async fn get_yt_general_data(
        &self,
        yt_token_address: Address,
        block_number: u64,
    ) -> Result<YtGeneralData> {
        let yield_token_instance =
            PendleYieldTokenInstance::new(yt_token_address, self.provider.clone());

        let multicall = self
            .provider
            .multicall()
            .add(yield_token_instance.isExpired())
            .add(yield_token_instance.syReserve())
            .add(yield_token_instance.factory());

        let (is_expired, sy_reserve, factory) =
            multicall.block(block_number.into()).aggregate().await?;

        Ok(YtGeneralData {
            is_expired,
            sy_reserve,
            factory,
        })
    }

    /// Fetches active balances for multiple addresses from a Pendle market
    pub async fn get_all_market_active_balances(
        &self,
        market: Address,
        addresses: &[Address],
        block_number: u64,
    ) -> Result<Vec<U256>> {
        let market_instance = PendleMarketInstance::new(market, self.provider.clone());

        let calls: Vec<Call> = addresses
            .iter()
            .map(|&address| Call {
                target: market,
                callData: market_instance
                    .activeBalance(address)
                    .calldata()
                    .to_vec()
                    .into(),
            })
            .collect();

        let result = self
            .instance
            .aggregate(calls)
            .call()
            .block(block_number.into())
            .await?;

        // Decode the return data
        let balances: Vec<U256> = result
            .returnData
            .into_iter()
            .map(|data| U256::from_be_bytes::<32>(data.as_ref().try_into().unwrap()))
            .collect();

        Ok(balances)
    }

    /// Fetches YT interest data for multiple addresses
    pub async fn get_all_yt_interest_data(
        &self,
        yt: Address,
        addresses: &[Address],
        block_number: u64,
    ) -> Result<Vec<YtInterestData>> {
        let yt_instance = PendleYieldTokenInstance::new(yt, self.provider.clone());

        let calls: Vec<Call> = addresses
            .iter()
            .map(|&address| Call {
                target: yt,
                callData: yt_instance.userInterest(address).calldata().to_vec().into(),
            })
            .collect();

        let results = self.try_aggregate_multicall(calls, block_number).await?;

        // Decode the return data - userInterest returns (uint128 index, uint128 accrue)
        let interests: Vec<YtInterestData> = results
            .into_iter()
            .map(|data| {
                if data.clone().is_empty() {
                    YtInterestData {
                        index: U256::ZERO,
                        accrue: U256::ZERO,
                    }
                } else if data.len() >= 64 {
                    // ABI encoding puts values sequentially in 32-byte slots
                    let index = U256::from_be_slice(&data[0..32]);
                    let accrue = U256::from_be_slice(&data[32..64]);

                    YtInterestData { index, accrue }
                } else {
                    // Fallback if data is too short
                    YtInterestData {
                        index: U256::from_be_slice(&data[0..32.min(data.len())]),
                        accrue: U256::ZERO,
                    }
                }
            })
            .collect();

        Ok(interests)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::providers::ProviderBuilder;
    use reqwest::Url;

    use super::*;

    const RPC_URL: &str = "https://rpc.hyperliquid.xyz/evm";

    lazy_static::lazy_static! {
        static ref YT_TOKEN_ADDRESS: Address =
            Address::from_str("0x3f6583ad479ab6020297ce12c9059d7480ab6e5a").unwrap();
        static ref USER_1: Address =
            Address::from_str("0xc328dfcd2c8450e2487a91daa9b75629075b7a43").unwrap();
        static ref USER_2: Address =
            Address::from_str("0x1fccc097db89a86bfc474a1028f93958295b1fb7").unwrap();
    }

    #[tokio::test]
    async fn test_get_all_erc20_balances() {
        let rpc_provider = ProviderBuilder::new().connect_http(Url::from_str(RPC_URL).unwrap());
        let multicall = Multicall::new(rpc_provider.clone());

        let block_number = rpc_provider.get_block_number().await.unwrap();

        let balances = multicall
            .get_all_erc20_balances(*YT_TOKEN_ADDRESS, &[*USER_1, *USER_2], block_number)
            .await
            .unwrap();

        println!("{:?}", balances);
    }

    #[tokio::test]
    async fn test_get_yt_general_data() {
        let rpc_provider = ProviderBuilder::new().connect_http(Url::from_str(RPC_URL).unwrap());
        let multicall = Multicall::new(rpc_provider.clone());

        let block_number = rpc_provider.get_block_number().await.unwrap();

        let yt_general_data = multicall
            .get_yt_general_data(*YT_TOKEN_ADDRESS, block_number)
            .await
            .unwrap();

        println!("{:?}", yt_general_data);
    }

    #[tokio::test]
    async fn test_get_all_erc20_balances_multi_tokens() {
        let rpc_provider = ProviderBuilder::new().connect_http(Url::from_str(RPC_URL).unwrap());
        let multicall = Multicall::new(rpc_provider.clone());

        let block_number = rpc_provider.get_block_number().await.unwrap();

        // Example: Get balances for different token-address pairs
        let tokens = vec![
            *YT_TOKEN_ADDRESS,
            *YT_TOKEN_ADDRESS, // Using same token for test, but could be different
        ];

        let addresses = vec![
            Address::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            Address::from_str("0x0000000000000000000000000000000000000002").unwrap(),
        ];

        let balances = multicall
            .get_all_erc20_balances_multi_tokens(&tokens, &addresses, block_number)
            .await
            .unwrap();

        println!("Balances: {:?}", balances);
        assert_eq!(balances.len(), 2);
    }

    lazy_static::lazy_static! {
        // Add a market address for testing
        static ref MARKET_ADDRESS: Address =
            Address::from_str("0x8867d2b7adb8609c51810237ecc9a25a2f601b97").unwrap();
    }

    #[tokio::test]
    async fn test_get_all_market_active_balances() {
        let rpc_provider = ProviderBuilder::new().connect_http(Url::from_str(RPC_URL).unwrap());
        let multicall = Multicall::new(rpc_provider.clone());

        let block_number = rpc_provider.get_block_number().await.unwrap();

        let addresses = vec![
            Address::from_str("0xc328dfcd2c8450e2487a91daa9b75629075b7a43").unwrap(),
            Address::from_str("0x1fccc097db89a86bfc474a1028f93958295b1fb7").unwrap(),
        ];

        let balances = multicall
            .get_all_market_active_balances(*MARKET_ADDRESS, &addresses, block_number)
            .await
            .unwrap();

        println!("Active balances: {:?}", balances);
        assert_eq!(balances.len(), addresses.len());
    }

    #[tokio::test]
    async fn test_get_all_yt_interest_data() {
        let rpc_provider = ProviderBuilder::new().connect_http(Url::from_str(RPC_URL).unwrap());
        let multicall = Multicall::new(rpc_provider.clone());

        let block_number = rpc_provider.get_block_number().await.unwrap();

        let addresses = vec![
            Address::from_str("0xc328dfcd2c8450e2487a91daa9b75629075b7a43").unwrap(),
            Address::from_str("0x1fccc097db89a86bfc474a1028f93958295b1fb7").unwrap(),
        ];

        let interests = multicall
            .get_all_yt_interest_data(*YT_TOKEN_ADDRESS, &addresses, block_number)
            .await
            .unwrap();

        println!("YT interest data: {:?}", interests);
        assert_eq!(interests.len(), addresses.len());

        for interest in &interests {
            println!("Index: {}, Accrue: {}", interest.index, interest.accrue);
        }
    }
}
