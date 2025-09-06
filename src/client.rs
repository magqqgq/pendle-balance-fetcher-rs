use std::{collections::HashMap, str::FromStr, time::Duration};

use alloy::primitives::Address;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    constants::HYPEREVM_CHAIN_ID,
    types::protocols::{
        EulerUserInstance, LiquidLockerData, MmType, MorphoUserInstance, SiloUserInstance,
    },
};

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct MmMapType {
    pub holder: Address,
    #[serde(rename = "type")]
    pub mm_type: MmType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WlpInfo {
    pub wlp: String,
    pub wlp_holders: Vec<Address>,
    pub euler: Vec<EulerUserInstance>,
    pub silo: Vec<SiloUserInstance>,
    pub morpho_address: Option<Address>,
    pub morpho: Vec<MorphoUserInstance>,
    #[serde(rename = "remapMMHolder")]
    pub remap_mm_holder: HashMap<Address, MmMapType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullMarketInfo {
    pub lp_holders: Vec<Address>,
    pub liquid_locker_datas: Vec<LiquidLockerData>,
    pub wlp_info: Option<WlpInfo>,
}

#[derive(Debug, Clone)]
pub struct PendleClient {
    client: Client,
}

impl PendleClient {
    const PENDLE_API_URL: &str = "https://api-v2.pendle.finance/core/v1/statistics";

    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    pub async fn query_token(&self, token: &Address) -> Result<Vec<Address>> {
        let url = format!(
            "{}/get-distinct-user-from-token?token={}",
            Self::PENDLE_API_URL,
            token.to_string().to_lowercase()
        );

        #[derive(Deserialize)]
        struct Response {
            users: Vec<String>,
        }

        let res: Response = self.client.get(&url).send().await?.json().await?;

        let users = res
            .users
            .iter()
            .map(|user| Address::from_str(user).unwrap())
            .collect();

        Ok(users)
    }

    pub async fn query_market_info(&self, market: &Address) -> Result<FullMarketInfo> {
        let url = format!(
            "{}/get-all-related-info-from-lp-and-wlp?chainId={}&marketAddress={}",
            Self::PENDLE_API_URL,
            HYPEREVM_CHAIN_ID,
            market.to_string().to_lowercase()
        );

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            distinct_users: Vec<Address>,
            liquid_locker_pools: Vec<LiquidLockerData>,
            wlp_distinct_users_response: Option<WlpDistinctUsersResponse>,
            #[serde(default)]
            wlp_holder_mappings: Vec<WlpHolderMapping>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WlpDistinctUsersResponse {
            wlp_address: String,
            wlp_users: Vec<String>,
            morpho_users: Vec<MorphoUserInstance>,
            euler_users: Vec<EulerUserInstance>,
            silo_users: Vec<SiloUserInstance>,
            morpho_configs: Vec<MorphoConfig>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MorphoConfig {
            morpho_address: String,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WlpHolderMapping {
            asset: String,
            holder: String,
            money_market: MmType,
        }

        let res: Response = self.client.get(&url).send().await?.json().await?;

        // If no `wlpDistinctUsersResponse` in response, return simple response
        if res.wlp_distinct_users_response.is_none() {
            return Ok(FullMarketInfo {
                lp_holders: res.distinct_users,
                liquid_locker_datas: res.liquid_locker_pools,
                wlp_info: None,
            });
        }

        // Build `remapMMHolder` map
        let mut remap_mm_holder: HashMap<Address, MmMapType> = HashMap::new();
        for whm in res.wlp_holder_mappings {
            let asset = match Address::from_str(&whm.asset) {
                Ok(asset) => asset,
                Err(_) => continue,
            };

            let holder = match Address::from_str(&whm.holder) {
                Ok(holder) => holder,
                Err(_) => continue,
            };

            remap_mm_holder.insert(
                asset,
                MmMapType {
                    holder,
                    mm_type: whm.money_market,
                },
            );
        }

        // Extract Morpho address if available
        let wlp_response = res.wlp_distinct_users_response.unwrap();
        let morpho_address = wlp_response
            .morpho_configs
            .first()
            .map(|config| config.morpho_address.to_lowercase());

        Ok(FullMarketInfo {
            lp_holders: res.distinct_users,
            liquid_locker_datas: res.liquid_locker_pools,
            wlp_info: Some(WlpInfo {
                wlp: wlp_response.wlp_address,
                wlp_holders: wlp_response
                    .wlp_users
                    .iter()
                    .map(|user| Address::from_str(user).unwrap())
                    .collect(),
                morpho: wlp_response.morpho_users,
                euler: wlp_response.euler_users,
                morpho_address: morpho_address.map(|address| Address::from_str(&address).unwrap()),
                silo: wlp_response.silo_users,
                remap_mm_holder,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[tokio::test]
    async fn test_query_token() {
        let client = PendleClient::new();
        let result = client
            .query_token(&Address::from_str("0x57fc55dff8ceca86ee94a6bf255af2f0ed90eb9e").unwrap())
            .await
            .unwrap();
        println!("{:?}", result);
    }
}
