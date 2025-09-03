pub mod client;
pub mod constants;
pub mod contracts;
pub mod multicall;
pub mod types;
pub mod utils;

use std::{collections::HashMap, str::FromStr};

use alloy::{
    primitives::{Address, BlockNumber, FixedBytes, U256},
    providers::{Provider, ProviderBuilder},
    transports::http::reqwest::Url,
};
use anyhow::Result;
use futures::future::try_join_all;
use rust_decimal::{Decimal, prelude::ToPrimitive};

use crate::{
    client::{FullMarketInfo, PendleClient},
    constants::PENDLE_TREASURY,
    contracts::{
        MorphoBlue::MorphoBlueInstance, Multicall3::Call, PendleMarket::PendleMarketInstance,
        PendleYieldContractFactory::PendleYieldContractFactoryInstance,
        PendleYieldToken::PendleYieldTokenInstance, SyToken::SyTokenInstance,
    },
    multicall::Multicall,
    types::{
        PoolConfig, PoolType, SnapshotResult, UserRecord, UserTempShare,
        protocols::{
            EulerUserInstance, LiquidLockerData, MmType, MorphoUserInstance, SiloUserInstance,
        },
        provider::RpcProvider,
    },
    utils::from_u256_to_decimal,
};

/// A client for fetching Pendle generic balances for a specific pool configuration.
#[derive(Clone)]
pub struct PendleBalanceFetcher {
    rpc_provider: RpcProvider,
    multicall: Multicall,
    client: PendleClient,
    pool_config: PoolConfig,
}

impl PendleBalanceFetcher {
    /// Creates a new `PendleBalanceFetcher` client.
    ///
    /// # Arguments
    ///
    /// * `rpc_url` - The URL of the Ethereum JSON-RPC endpoint.
    /// * `config` - The pool configuration for this client instance.
    ///
    /// # Returns
    ///
    /// A `Result` containing a new `PendleBalanceFetcher` instance or a `FetcherError`.
    /// Creates a new `PendleBalanceFetcher` client.
    pub fn try_new(rpc_url: &Url, pool_config: PoolConfig) -> Result<Self> {
        let rpc_provider = ProviderBuilder::new().connect_http(rpc_url.clone());
        let multicall = Multicall::new(rpc_provider.clone());
        let client = PendleClient::new();

        Ok(Self {
            rpc_provider,
            client,
            multicall,
            pool_config,
        })
    }

    pub async fn fetch_user_balance_snapshot_batch(
        &self,
        block_numbers: &[BlockNumber],
        pool_type: PoolType,
    ) -> Result<Vec<SnapshotResult>> {
        let all_yt_users = self.client.query_token(&self.pool_config.yt).await?;

        let lp_infos = try_join_all(
            self.pool_config
                .lps
                .iter()
                .map(|lp| self.client.query_market_info(&lp.address)),
        )
        .await?;

        let tasks = block_numbers.iter().map(|&block_number| {
            let yt_users = all_yt_users.clone();
            let lp_infos = lp_infos.clone();
            let fetcher = self.clone();

            async move {
                match pool_type {
                    PoolType::Shares => {
                        fetcher
                            .fetch_user_balance_snapshot(yt_users, lp_infos, block_number)
                            .await
                    }
                    PoolType::LpValueInSy => {
                        fetcher
                            .fetch_user_lp_value_in_sy_snapshot(lp_infos, block_number)
                            .await
                    }
                }
            }
        });

        let sy_snapshots = try_join_all(tasks).await?;

        self.fetch_and_add_underlying_balances(&sy_snapshots).await
    }

    async fn fetch_and_add_underlying_balances(
        &self,
        snapshots: &[SnapshotResult],
    ) -> Result<Vec<SnapshotResult>> {
        let sy_contract = SyTokenInstance::new(self.pool_config.sy, self.rpc_provider.clone());

        let exchange_rate_futures = snapshots.iter().map(|snapshot| {
            let sy_contract = sy_contract.clone();
            async move {
                sy_contract
                    .exchangeRate()
                    .call()
                    .block(snapshot.block_number.into())
                    .await
                    .map(|r| r)
            }
        });

        let exchange_rates = try_join_all(exchange_rate_futures).await?;

        let new_snapshots = snapshots
            .iter()
            .zip(exchange_rates.iter())
            .map(|(snapshot, &exchange_rate)| {
                let underlying_yt = self.convert_sy_user_record_to_underlying(
                    &snapshot.yt_user_records_in_sy,
                    exchange_rate,
                );

                let underlying_lp = self.convert_sy_user_record_to_underlying(
                    &snapshot.lp_user_records_in_sy,
                    exchange_rate,
                );

                SnapshotResult {
                    block_number: snapshot.block_number,
                    yt_user_records_in_sy: snapshot.yt_user_records_in_sy.clone(),
                    lp_user_records_in_sy: snapshot.lp_user_records_in_sy.clone(),
                    yt_user_records_in_underlying: underlying_yt,
                    lp_user_records_in_underlying: underlying_lp,
                }
            })
            .collect();

        Ok(new_snapshots)
    }

    fn convert_sy_user_record_to_underlying(
        &self,
        sy_record: &UserRecord,
        exchange_rate: U256,
    ) -> UserRecord {
        sy_record
            .iter()
            .map(|(user, sy_balance)| {
                let underlying_balance = *sy_balance * exchange_rate / U256::from(1e18);
                (*user, underlying_balance)
            })
            .filter(|(_, balance)| *balance > U256::ZERO)
            .collect()
    }

    async fn fetch_user_balance_snapshot(
        &self,
        all_yt_users: Vec<Address>,
        lp_infos: Vec<FullMarketInfo>,
        block_number: u64,
    ) -> Result<SnapshotResult> {
        let mut yt_user_records = UserRecord::default();
        let mut lp_user_records = UserRecord::default();

        // Apply YT holder shares
        self.apply_yt_holder_shares(&mut yt_user_records, &all_yt_users, block_number)
            .await?;

        // Apply LP holder shares
        let lp_futures = lp_infos
            .into_iter()
            .zip(self.pool_config.lps.iter())
            .filter(|(_, lp_market)| lp_market.deployed_block <= block_number)
            .map(|(lp_info, lp_market)| {
                let fetcher = self.clone();
                async move {
                    let mut temp_result = UserRecord::default();
                    fetcher
                        .apply_lp_holder_shares(
                            &mut temp_result,
                            lp_market.address,
                            &lp_info,
                            block_number,
                        )
                        .await?;
                    Ok::<UserRecord, anyhow::Error>(temp_result)
                }
            });

        let lp_results = try_join_all(lp_futures).await?;

        for temp_result in lp_results {
            for (user, amount) in temp_result {
                *lp_user_records.entry(user).or_insert(U256::ZERO) += amount;
            }
        }

        let mut yt_user_records_vec: Vec<_> = yt_user_records.into_iter().collect();
        yt_user_records_vec.sort_by(|a, b| b.1.cmp(&a.1));

        let mut lp_user_records_vec: Vec<_> = lp_user_records.into_iter().collect();
        lp_user_records_vec.sort_by(|a, b| b.1.cmp(&a.1));

        Ok(SnapshotResult {
            block_number,
            yt_user_records_in_sy: yt_user_records_vec.into_iter().collect(),
            lp_user_records_in_sy: lp_user_records_vec.into_iter().collect(),
            yt_user_records_in_underlying: UserRecord::default(),
            lp_user_records_in_underlying: UserRecord::default(),
        })
    }

    async fn fetch_user_lp_value_in_sy_snapshot(
        &self,
        lp_infos: Vec<FullMarketInfo>,
        block_number: u64,
    ) -> Result<SnapshotResult> {
        let mut lp_user_records = UserRecord::default();

        // Process each LP market
        for (i, lp_market) in self.pool_config.lps.iter().enumerate() {
            if lp_market.deployed_block <= block_number {
                if let Some(lp_info) = lp_infos.get(i) {
                    let lp_value_record = self
                        .apply_lp_holder_values_in_sy(
                            lp_market.address,
                            self.pool_config.yt,
                            &lp_info.lp_holders,
                            &lp_info.liquid_locker_datas,
                            block_number,
                        )
                        .await?;

                    // Merge results
                    for (user, amount) in lp_value_record {
                        *lp_user_records.entry(user).or_insert(U256::ZERO) += amount;
                    }
                }
            }
        }

        let mut lp_user_records_vec: Vec<_> = lp_user_records.into_iter().collect();
        lp_user_records_vec.sort_by(|a, b| b.1.cmp(&a.1));

        Ok(SnapshotResult {
            block_number,
            yt_user_records_in_sy: UserRecord::default(),
            lp_user_records_in_sy: lp_user_records_vec.into_iter().collect(),
            yt_user_records_in_underlying: UserRecord::default(),
            lp_user_records_in_underlying: UserRecord::default(),
        })
    }

    async fn apply_yt_holder_shares(
        &self,
        yt_user_records: &mut UserRecord,
        all_yt_users: &[Address],
        block_number: u64,
    ) -> Result<()> {
        println!("YT users count: {}", all_yt_users.len()); // Debug

        println!("block_number: {}", block_number);
        // Get YT token general data
        let general_data = self
            .multicall
            .get_yt_general_data(self.pool_config.yt, block_number)
            .await?;

        let is_expired = general_data.is_expired;
        let sy_reserve = general_data.sy_reserve;
        let factory = general_data.factory;

        println!("YT is expired: {}", is_expired);
        println!("SY reserve: {}", sy_reserve);
        println!("Factory: {}", factory);

        // If expired, add SY reserve to Pendle treasury
        if general_data.is_expired {
            self.increase_user_amount(yt_user_records, *PENDLE_TREASURY, sy_reserve);
            return Ok(());
        }

        // Get balances and interest data for all users
        let (balances_raw, yt_interests_raw) = tokio::try_join!(
            self.multicall
                .get_all_erc20_balances(self.pool_config.yt, all_yt_users, block_number),
            self.multicall.get_all_yt_interest_data(
                self.pool_config.yt,
                all_yt_users,
                block_number
            )
        )?;

        println!("Balances fetched: {}", balances_raw.len()); // Debug
        println!(
            "Non-zero balances: {}",
            balances_raw.iter().filter(|&&b| b > U256::ZERO).count()
        ); // Debug

        // Map balances with users
        let users_balances: Vec<(Address, U256)> = all_yt_users
            .iter()
            .zip(balances_raw.iter())
            .map(|(&user, &balance)| (user, balance))
            .collect();

        // Map interests with users
        let users_interests: Vec<(Address, U256, U256)> = all_yt_users
            .iter()
            .zip(yt_interests_raw.iter())
            .map(|(&user, interest)| (user, interest.index, interest.accrue))
            .collect();

        // Calculate YTIndex as max of all user indices
        let users_yt_index = users_interests
            .iter()
            .map(|(_, index, _)| *index)
            .max()
            .unwrap_or(U256::ZERO);

        println!("YT Index: {:?}", users_yt_index); // Debug

        if users_yt_index == U256::ZERO {
            println!("YT Index is zero, returning early"); // Debug
            return Ok(());
        }

        // Track YT balances for interest calculation
        let mut users_yt_balances: HashMap<Address, U256> = HashMap::new();

        // Get fee rate from factory
        let fee_rate = self
            .get_factory_fee_rate(general_data.factory, block_number)
            .await?;

        println!("Fee rate: {:?}", fee_rate); // Debug
        println!("Processing {} balances", users_balances.len()); // Debug

        // Process balances
        for (user, balance) in users_balances {
            let implied_balance = U256::from(1e18) * balance / users_yt_index;
            let fee_share = implied_balance * fee_rate / U256::from(1e18);
            let remaining = implied_balance - fee_share;

            self.increase_user_amount(yt_user_records, user, remaining);
            self.increase_user_amount(
                yt_user_records,
                *crate::constants::PENDLE_TREASURY,
                fee_share,
            );
            users_yt_balances.insert(user, balance);
        }

        println!(
            "YT records after balance processing: {}",
            yt_user_records.len()
        ); // Debug

        // Process interests
        for (user, user_index, amount) in users_interests {
            if user == self.pool_config.yt {
                continue;
            }
            if user_index == U256::ZERO {
                continue;
            }

            let user_balance = users_yt_balances.get(&user).copied().unwrap_or(U256::ZERO);
            if user_balance == U256::ZERO {
                continue;
            }

            let pending_interest = user_balance * (users_yt_index - user_index) * U256::from(1e18)
                / (users_yt_index * user_index);

            let total_interest = pending_interest + amount;
            self.increase_user_amount(yt_user_records, user, total_interest);
        }

        Ok(())
    }

    async fn apply_lp_holder_shares(
        &self,
        result: &mut UserRecord,
        lp_token_address: Address,
        lp_info: &FullMarketInfo,
        block_number: u64,
    ) -> Result<()> {
        // Get total SY balance held by the LP token
        let total_sy = self
            .multicall
            .get_all_erc20_balances(self.pool_config.sy, &[lp_token_address], block_number)
            .await?[0];

        // Get all active balances for LP holders
        let all_active_balances = self
            .multicall
            .get_all_market_active_balances(lp_token_address, &lp_info.lp_holders, block_number)
            .await?;

        // Calculate total active supply
        let total_active_supply = all_active_balances
            .iter()
            .fold(U256::ZERO, |acc, balance| acc + balance);

        if total_active_supply == U256::ZERO {
            return Ok(());
        }

        // Process each LP holder
        for (i, holder) in lp_info.lp_holders.iter().enumerate() {
            let active_balance = all_active_balances[i];

            // Calculate boosted SY balance (proportional share)
            let boosted_sy_balance = active_balance * total_sy / total_active_supply;

            // Check if this is a WLP holder
            if let Some(wlp_info) = &lp_info.wlp_info {
                if holder.to_string().to_lowercase() == wlp_info.wlp.to_lowercase() {
                    // TODO: Implement applyWlpHolderShares
                    self.apply_wlp_holder_shares(result, lp_info, block_number, boosted_sy_balance)
                        .await?;
                    continue;
                }
            }

            // Check if this is a liquid locker
            let ll_index = lp_info.liquid_locker_datas.iter().position(|data| {
                data.lp_holder.to_string().to_lowercase() == holder.to_string().to_lowercase()
            });

            if ll_index.is_none() {
                // Regular holder - add balance directly
                self.increase_user_amount(result, *holder, boosted_sy_balance);
            } else {
                // Liquid locker - resolve shares
                let shares = self
                    .resolve_liquid_locker(
                        boosted_sy_balance,
                        &lp_info.liquid_locker_datas[ll_index.unwrap()],
                        block_number,
                    )
                    .await?;
                self.increase_user_amounts(result, &shares);
            }
        }

        Ok(())
    }

    // Add these placeholder methods that need implementation:
    async fn apply_wlp_holder_shares(
        &self,
        result: &mut UserRecord,
        lp_info: &FullMarketInfo,
        block_number: u64,
        boosted_sy_balance: U256,
    ) -> Result<()> {
        let wlp_info = match &lp_info.wlp_info {
            Some(info) => info,
            None => return Ok(()),
        };

        // Convert string addresses to Address type
        let wlp_address = Address::from_str(&wlp_info.wlp)?;
        let wlp_holder_addresses: Vec<Address> = wlp_info
            .wlp_holders
            .iter()
            .map(|h| Address::from_str(h))
            .collect::<Result<Vec<_>, _>>()?;

        // Get balances for all WLP holders
        let balances = self
            .multicall
            .get_all_erc20_balances(wlp_address, &wlp_holder_addresses, block_number)
            .await?;

        // Calculate total supply
        let total_supply = balances.iter().fold(U256::ZERO, |acc, &b| acc + b);

        if total_supply == U256::ZERO {
            return Ok(());
        }

        // Calculate SY per one WLP token (scaled by 1e18)
        let sy_per_one_wlp = boosted_sy_balance * U256::from(1e18) / total_supply;

        // Track MM shares by type
        let mut total_mm_shares: HashMap<MmType, U256> = HashMap::new();

        // Process each WLP holder
        for (i, holder) in wlp_info.wlp_holders.iter().enumerate() {
            let wlp_balance = balances[i];
            let user_share = wlp_balance * sy_per_one_wlp / U256::from(1e18);

            // Check if this holder is a money market
            let mm_type = self.get_mm_type(lp_info, holder);

            if let Some(mm_type) = mm_type {
                *total_mm_shares.entry(mm_type).or_insert(U256::ZERO) += user_share;
            } else {
                // Regular holder
                let holder_address = Address::from_str(holder)?;
                self.increase_user_amount(result, holder_address, user_share);
            }
        }

        // Resolve money market shares in parallel
        let (euler_shares, silo_shares, morpho_shares) = tokio::try_join!(
            self.resolve_euler(sy_per_one_wlp, &wlp_info.euler, block_number),
            self.resolve_silo(sy_per_one_wlp, &wlp_info.silo, block_number),
            async {
                if let Some(morpho_address) = &wlp_info.morpho_address {
                    self.resolve_morpho(
                        sy_per_one_wlp,
                        *morpho_address,
                        &wlp_info.morpho,
                        block_number,
                    )
                    .await
                } else {
                    Ok(vec![])
                }
            }
        )?;

        // Perform soft checks
        self.soft_check(
            &euler_shares,
            total_mm_shares
                .get(&MmType::Euler)
                .copied()
                .unwrap_or(U256::ZERO),
        )?;
        self.soft_check(
            &silo_shares,
            total_mm_shares
                .get(&MmType::Silo)
                .copied()
                .unwrap_or(U256::ZERO),
        )?;
        self.soft_check(
            &morpho_shares,
            total_mm_shares
                .get(&MmType::Morpho)
                .copied()
                .unwrap_or(U256::ZERO),
        )?;

        // Add all shares to result
        self.increase_user_amounts(result, &euler_shares);
        self.increase_user_amounts(result, &silo_shares);
        self.increase_user_amounts(result, &morpho_shares);

        Ok(())
    }

    fn get_mm_type(&self, lp_info: &FullMarketInfo, holder: &str) -> Option<MmType> {
        lp_info
            .wlp_info
            .as_ref()?
            .remap_mm_holder
            .get(&holder.to_lowercase())
            .map(|mm_map| mm_map.mm_type.clone())
    }

    fn soft_check(&self, shares: &[UserTempShare], upperbound: U256) -> Result<()> {
        let total = shares
            .iter()
            .fold(U256::ZERO, |acc, share| acc + share.share);

        if total > upperbound {
            return Err(anyhow::anyhow!(
                "Total shares {} exceeds upper bound {}",
                total,
                upperbound
            ));
        }
        Ok(())
    }

    async fn resolve_euler(
        &self,
        sy_per_one_wlp: U256,
        euler_users: &[EulerUserInstance],
        block_number: u64,
    ) -> Result<Vec<UserTempShare>> {
        if euler_users.is_empty() {
            return Ok(vec![]);
        }

        let assets: Vec<Address> = euler_users.iter().map(|eui| eui.asset).collect();
        let users: Vec<Address> = euler_users.iter().map(|eui| eui.user).collect();

        let balances = self
            .multicall
            .get_all_erc20_balances_multi_tokens(&assets, &users, block_number)
            .await?;

        // Calculate shares
        let mut user_temp_shares = Vec::new();
        for (i, balance) in balances.iter().enumerate() {
            user_temp_shares.push(UserTempShare {
                user: users[i],
                share: balance * sy_per_one_wlp / U256::from(1e18),
            });
        }

        Ok(user_temp_shares)
    }

    async fn resolve_liquid_locker(
        &self,
        boosted_sy_balance: U256,
        liquid_locker_data: &LiquidLockerData,
        block_number: u64,
    ) -> Result<Vec<UserTempShare>> {
        if boosted_sy_balance == U256::ZERO {
            return Ok(vec![]);
        }

        let receipt_token = liquid_locker_data.receipt_token;
        let users = &liquid_locker_data.users;

        // Get receipt token balances for all users
        let balances = self
            .multicall
            .get_all_erc20_balances(receipt_token, users, block_number)
            .await?;

        // Calculate total receipt balance
        let total_receipt_balance = balances.iter().fold(U256::ZERO, |acc, &b| acc + b);

        if total_receipt_balance == U256::ZERO {
            return Ok(vec![]);
        }

        // Calculate shares for each user
        let mut user_temp_shares = Vec::new();
        for (j, user) in users.iter().enumerate() {
            let receipt_balance = balances[j];

            if receipt_balance == U256::ZERO {
                continue;
            }

            let user_share = receipt_balance * boosted_sy_balance / total_receipt_balance;

            user_temp_shares.push(UserTempShare {
                user: *user,
                share: user_share,
            });
        }

        Ok(user_temp_shares)
    }

    async fn resolve_silo(
        &self,
        sy_per_one_wlp: U256,
        silo_users: &[SiloUserInstance],
        block_number: u64,
    ) -> Result<Vec<UserTempShare>> {
        if silo_users.is_empty() {
            return Ok(vec![]);
        }

        const SILO_DECIMALS_OFFSET: u64 = 1000;

        // Convert string addresses to Address type
        let assets = silo_users.iter().map(|sui| sui.asset).collect::<Vec<_>>();
        let users = silo_users.iter().map(|sui| sui.user).collect::<Vec<_>>();

        // Get balances for all asset-user pairs
        let balances = self
            .multicall
            .get_all_erc20_balances_multi_tokens(&assets, &users, block_number)
            .await?;

        // Calculate shares
        let mut user_temp_shares = Vec::new();
        for (i, balance) in balances.iter().enumerate() {
            user_temp_shares.push(UserTempShare {
                user: users[i],
                share: balance * sy_per_one_wlp
                    / U256::from(1e18)
                    / U256::from(SILO_DECIMALS_OFFSET),
            });
        }

        Ok(user_temp_shares)
    }

    async fn resolve_morpho(
        &self,
        sy_per_one_wlp: U256,
        morpho_address: Address,
        morpho_users: &[MorphoUserInstance],
        block_number: u64,
    ) -> Result<Vec<UserTempShare>> {
        if morpho_users.is_empty() {
            return Ok(vec![]);
        }

        // Create MorphoBlue contract instance
        let morpho_instance = MorphoBlueInstance::new(morpho_address, self.rpc_provider.clone());

        // Build calls for each user's position
        let calls: Vec<contracts::Multicall3::Call> = morpho_users
            .iter()
            .map(|mui| {
                // Convert market_id from String to FixedBytes<32>
                let market_id = FixedBytes::<32>::from_str(&mui.market_id).unwrap();

                // Create call data for position(marketId, user)
                let caliquid_locker_data = morpho_instance
                    .position(market_id, mui.user)
                    .calldata()
                    .to_vec();

                Call {
                    target: morpho_address,
                    callData: caliquid_locker_data.into(),
                }
            })
            .collect();

        // Execute multicall
        let results = self
            .multicall
            .try_aggregate_multicall(calls, block_number)
            .await?;

        // Process results and calculate shares
        let mut user_temp_shares = Vec::new();
        for (i, data) in results.iter().enumerate() {
            let collateral = if data.is_empty() {
                U256::ZERO
            } else {
                // The position function returns multiple values, collateral is the first
                // Decode as tuple (uint256 collateral, uint256 supplyShares, uint256 borrowShares)
                // We only need collateral which is the first 32 bytes
                U256::from_be_bytes::<32>(data[0..32].try_into().unwrap_or([0u8; 32]))
            };

            user_temp_shares.push(UserTempShare {
                user: morpho_users[i].user,
                share: collateral * sy_per_one_wlp / U256::from(1e18 as u64),
            });
        }

        Ok(user_temp_shares)
    }

    // Also add the missing apply_lp_holder_values_in_sy method:
    async fn apply_lp_holder_values_in_sy(
        &self,
        lp_market: Address,
        yt: Address,
        lp_holders: &[Address],
        liquid_locker_datas: &[LiquidLockerData],
        block_number: u64,
    ) -> Result<UserRecord> {
        let mut result = UserRecord::new();

        // Get balances for all LP holders
        let balances = self
            .multicall
            .get_all_erc20_balances(lp_market, lp_holders, block_number)
            .await?;

        // Get LP to SY exchange rate
        let price = self.get_lp_to_sy_rate(lp_market, yt, block_number).await?;

        // Process each LP holder
        for (i, holder) in lp_holders.iter().enumerate() {
            let balance = balances[i];

            // Find if this holder is a liquid locker
            let ll_index = liquid_locker_datas.iter().position(|data| {
                data.lp_holder.to_string().to_lowercase() == holder.to_string().to_lowercase()
            });

            if ll_index.is_none() {
                // Regular holder - add their value directly
                let value = balance * price / U256::from(1e18);
                self.increase_user_amount(&mut result, *holder, value);
            } else {
                // Liquid locker - distribute to receipt token holders
                let liquid_locker_data = &liquid_locker_datas[ll_index.unwrap()];
                let users = liquid_locker_data.users.clone();

                // Convert user addresses
                let receipt_token = liquid_locker_data.receipt_token;
                let user_addresses = users.into_iter().collect::<Vec<_>>();

                // Get receipt token balances
                let receipt_balances = self
                    .multicall
                    .get_all_erc20_balances(receipt_token, &user_addresses, block_number)
                    .await?;

                // Calculate total receipt balance
                let total_receipt_balance =
                    receipt_balances.iter().fold(U256::ZERO, |acc, &b| acc + b);

                if total_receipt_balance == U256::ZERO {
                    continue;
                }

                // Distribute to receipt token holders
                for (j, user_addr) in user_addresses.iter().enumerate() {
                    let receipt_balance = receipt_balances[j];

                    if receipt_balance == U256::ZERO {
                        continue;
                    }

                    // Calculate user's share
                    let user_share = receipt_balance * balance * price
                        / total_receipt_balance
                        / U256::from(1e18);

                    self.increase_user_amount(&mut result, *user_addr, user_share);
                }
            }
        }

        Ok(result)
    }

    /// Get factory fee rate
    async fn get_factory_fee_rate(&self, factory: Address, block_number: u64) -> Result<U256> {
        let factory_instance =
            PendleYieldContractFactoryInstance::new(factory, self.rpc_provider.clone());

        let rate = factory_instance
            .rewardFeeRate()
            .call()
            .block(block_number.into())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        Ok(U256::from(rate))
    }

    async fn get_lp_to_sy_rate(
        &self,
        lp_market: Address,
        yt: Address,
        block_number: u64,
    ) -> Result<U256> {
        let market = PendleMarketInstance::new(lp_market, self.rpc_provider.clone());
        let yt_contract = PendleYieldTokenInstance::new(yt, self.rpc_provider.clone());

        // Read market state
        let state = market
            .readState(lp_market)
            .call()
            .block(block_number.into())
            .await?;

        // Get block timestamp
        let block = self
            .rpc_provider
            .get_block_by_number(block_number.into())
            .await?;
        let block_timestamp = block.unwrap().header.timestamp;

        // Calculate time to expiry (in seconds)
        let time_to_expiry = state.expiry.saturating_sub(U256::from(block_timestamp));

        // Calculate PT to asset rate
        let time_to_expiry_u64: u64 = time_to_expiry.try_into().unwrap_or(0);
        let pt_to_asset_rate = self
            .get_exchange_rate_from_ln_implied_rate(state.lastLnImpliedRate, time_to_expiry_u64);

        println!("pt_to_asset_rate: {:?}", pt_to_asset_rate);

        // Get pyIndexCurrent
        let py_index = yt_contract
            .pyIndexCurrent()
            .call()
            .block(block_number.into())
            .await?;

        // Calculate PT to SY rate
        let pt_to_sy_rate = pt_to_asset_rate * U256::from(1e18) / py_index;

        // Calculate total value in SY
        let total_value_in_sy = U256::try_from(state.totalSy).unwrap_or(U256::ZERO)
            + (U256::try_from(state.totalPt).unwrap_or(U256::ZERO) * pt_to_sy_rate
                / U256::from(1e18));

        // Return rate
        Ok(total_value_in_sy * U256::from(1e18)
            / U256::try_from(state.totalLp).unwrap_or(U256::from(1)))
    }

    fn get_exchange_rate_from_ln_implied_rate(
        &self,
        ln_implied_rate: U256,
        time_to_expiry: u64,
    ) -> U256 {
        // Convert ln_implied_rate from U256 to f64 (assuming 18 decimals)
        let ln_rate_f64 = from_u256_to_decimal(ln_implied_rate)
            .unwrap_or(Decimal::ZERO)
            .to_f64()
            .unwrap_or(0.0);

        // Calculate normalized rate
        let one_year = crate::constants::ONE_YEAR as f64;
        let normalized_rate = (ln_rate_f64 * time_to_expiry as f64) / one_year;

        // Calculate e^normalized_rate
        let exp_rate = normalized_rate.exp();

        let exp_rate_scaled = (exp_rate * 1e18) as u128;

        // Convert back to U256 with 18 decimals
        U256::from(exp_rate_scaled)
    }

    /// Increase a user's amount in the result map.
    fn increase_user_amount(&self, result: &mut UserRecord, user: Address, amount: U256) {
        *result.entry(user).or_insert(U256::ZERO) += amount;
    }

    /// Increase multiple users' amounts using UserTempShare data
    fn increase_user_amounts(&self, result: &mut UserRecord, datas: &[UserTempShare]) {
        for data in datas {
            self.increase_user_amount(result, data.user, data.share);
        }
    }
}

#[cfg(test)]
mod tests {

    use std::str::FromStr;

    use alloy::primitives::Address;
    use lazy_static::lazy_static;

    use crate::types::Market;

    use super::*;

    const RPC_URL: &str = "https://rpc.hyperliquid.xyz/evm";

    lazy_static! {
        static ref KHYPE_SY: Address =
            Address::from_str("0x57fc55dff8ceca86ee94a6bf255af2f0ed90eb9e").unwrap();
        static ref KHYPE_YT: Address =
            Address::from_str("0x3f6583ad479ab6020297ce12c9059d7480ab6e5a").unwrap();
        static ref KHYPE_LPS: Vec<Market> = vec![Market {
            address: Address::from_str("0x8867d2b7adb8609c51810237ecc9a25a2f601b97").unwrap(),
            deployed_block: 9691348,
        },];
    }

    fn setup() -> PendleBalanceFetcher {
        let pool_config = PoolConfig {
            sy: KHYPE_SY.clone(),
            yt: KHYPE_YT.clone(),
            lps: KHYPE_LPS.clone(),
        };

        PendleBalanceFetcher::try_new(&Url::from_str(RPC_URL).unwrap(), pool_config).unwrap()
    }

    #[tokio::test]
    async fn test_fetch_user_balance_snapshot_batch() {
        let fetcher = setup();
        let blocks = [11474000, 11474100];

        let results = fetcher
            .fetch_user_balance_snapshot_batch(&blocks, PoolType::Shares)
            .await
            .unwrap();

        println!("result: {:?}", results);
    }
}
