pub mod builder;
pub mod client;
pub mod constants;
pub mod contracts;
pub mod multicall;
pub mod types;
pub mod utils;

pub use builder::PendleBalanceFetcherBuilder;

use std::{collections::HashMap, str::FromStr};

use alloy::{
    eips::BlockId,
    primitives::{Address, BlockNumber, FixedBytes, U256},
    providers::Provider,
};
use anyhow::Result;
use futures::future::try_join_all;
use rust_decimal::{Decimal, prelude::ToPrimitive};

use crate::{
    builder::MissingProvider,
    client::{FullMarketInfo, PendleClient},
    constants::PENDLE_TREASURY,
    contracts::{
        MorphoBlue::MorphoBlueInstance, Multicall3::Call, PendleMarket::PendleMarketInstance,
        PendleYieldContractFactory::PendleYieldContractFactoryInstance,
        PendleYieldToken::PendleYieldTokenInstance, SyToken::SyTokenInstance,
    },
    multicall::Multicall,
    types::{
        PoolConfig, PoolType, SnapshotResult, UserBalance, UserTempShare,
        protocols::{
            EulerUserInstance, LiquidLockerData, MmType, MorphoUserInstance, SiloUserInstance,
        },
        provider::RpcProvider,
    },
    utils::{from_u256_to_decimal, get_mm_type, sy_balances_to_underlying, WAD},
};

/// A client for fetching Pendle generic balances for a specific pool configuration.
#[derive(Clone)]
pub struct PendleBalanceFetcher {
    rpc_provider: RpcProvider,
    multicall: Multicall,
    client: PendleClient,
}

impl PendleBalanceFetcher {
    pub fn builder() -> PendleBalanceFetcherBuilder<MissingProvider> {
        PendleBalanceFetcherBuilder::new()
    }

    pub async fn fetch_user_balance_snapshot_batch(
        &self,
        pool_config: &PoolConfig,
        block_numbers: &[BlockNumber],
        pool_type: PoolType,
    ) -> Result<Vec<SnapshotResult>> {
        let all_yt_users = self.client.query_token(&pool_config.yt).await?;

        let lp_infos = try_join_all(
            pool_config
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
                            .fetch_user_balance_snapshot(
                                pool_config,
                                yt_users,
                                lp_infos,
                                block_number,
                            )
                            .await
                    }
                    PoolType::LpValueInSy => {
                        fetcher
                            .fetch_user_lp_value_in_sy_snapshot(pool_config, lp_infos, block_number)
                            .await
                    }
                }
            }
        });

        let sy_snapshots = try_join_all(tasks).await?;

        self.fetch_and_add_underlying_balances(pool_config, &sy_snapshots)
            .await
    }

    async fn fetch_and_add_underlying_balances(
        &self,
        pool_config: &PoolConfig,
        snapshots: &[SnapshotResult],
    ) -> Result<Vec<SnapshotResult>> {
        let sy_contract = SyTokenInstance::new(pool_config.sy, self.rpc_provider.clone());

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
                let yt_user_balances_in_sy = snapshot.yt_user_records_in_sy.clone();
                let lp_user_balances_in_sy = snapshot.lp_user_records_in_sy.clone();

                let yt_user_balances_in_underlying =
                    sy_balances_to_underlying(&yt_user_balances_in_sy, exchange_rate);
                let lp_user_balances_in_underlying =
                    sy_balances_to_underlying(&lp_user_balances_in_sy, exchange_rate);

                SnapshotResult {
                    block_timestamp: snapshot.block_timestamp,
                    block_number: snapshot.block_number,
                    yt_user_records_in_sy: yt_user_balances_in_sy,
                    lp_user_records_in_sy: lp_user_balances_in_sy,
                    yt_user_records_in_underlying: yt_user_balances_in_underlying,
                    lp_user_records_in_underlying: lp_user_balances_in_underlying,
                }
            })
            .collect();

        Ok(new_snapshots)
    }

    async fn fetch_user_balance_snapshot(
        &self,
        pool_config: &PoolConfig,
        all_yt_users: Vec<Address>,
        lp_infos: Vec<FullMarketInfo>,
        block_number: u64,
    ) -> Result<SnapshotResult> {
        let mut yt_user_records = UserBalance::default();
        let mut lp_user_records = UserBalance::default();

        // Apply YT holder shares
        self.apply_yt_holder_shares(
            pool_config.yt,
            &mut yt_user_records,
            &all_yt_users,
            block_number,
        )
        .await?;

        // Apply LP holder shares
        let lp_futures = lp_infos
            .into_iter()
            .zip(pool_config.lps.iter())
            .filter(|(_, lp_market)| lp_market.deployed_block <= block_number)
            .map(|(lp_info, lp_market)| {
                let fetcher = self.clone();
                async move {
                    let mut temp_result = UserBalance::default();
                    fetcher
                        .apply_lp_holder_shares(
                            &mut temp_result,
                            pool_config.sy,
                            lp_market.address,
                            &lp_info,
                            block_number,
                        )
                        .await?;
                    Ok::<UserBalance, anyhow::Error>(temp_result)
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

        let block = self
            .rpc_provider
            .get_block(BlockId::from(block_number))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Block not found"))?;
        let block_timestamp = block.header.timestamp;

        Ok(SnapshotResult {
            block_timestamp,
            block_number,
            yt_user_records_in_sy: yt_user_records_vec.into_iter().collect(),
            lp_user_records_in_sy: lp_user_records_vec.into_iter().collect(),
            yt_user_records_in_underlying: UserBalance::default(),
            lp_user_records_in_underlying: UserBalance::default(),
        })
    }

    async fn fetch_user_lp_value_in_sy_snapshot(
        &self,
        pool_config: &PoolConfig,
        lp_infos: Vec<FullMarketInfo>,
        block_number: u64,
    ) -> Result<SnapshotResult> {
        let block = self
            .rpc_provider
            .get_block(BlockId::from(block_number))
            .await?
            .ok_or_else(|| anyhow::anyhow!("Block not found"))?;

        let block_timestamp = block.header.timestamp;

        let mut lp_user_records = UserBalance::default();

        // Process each LP market
        for (i, lp_market) in pool_config.lps.iter().enumerate() {
            if lp_market.deployed_block <= block_number {
                if let Some(lp_info) = lp_infos.get(i) {
                    let lp_value_record = self
                        .apply_lp_holder_values_in_sy(
                            lp_market.address,
                            pool_config.yt,
                            &lp_info.lp_holders,
                            &lp_info.liquid_locker_datas,
                            block_number,
                            block_timestamp,
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
            block_timestamp,
            block_number,
            yt_user_records_in_sy: UserBalance::default(),
            lp_user_records_in_sy: lp_user_records_vec.into_iter().collect(),
            yt_user_records_in_underlying: UserBalance::default(),
            lp_user_records_in_underlying: UserBalance::default(),
        })
    }

    async fn apply_yt_holder_shares(
        &self,
        yt_token_address: Address,
        yt_user_records: &mut UserBalance,
        all_yt_users: &[Address],
        block_number: u64,
    ) -> Result<()> {
        // Fetch YT token metadata (expiry, reserves, factory) for this block.
        let general_data = self
            .multicall
            .get_yt_general_data(yt_token_address, block_number)
            .await?;

        // If expired, add SY reserve to Pendle treasury
        if general_data.is_expired {
            self.increase_user_amount(yt_user_records, *PENDLE_TREASURY, general_data.sy_reserve);
            return Ok(());
        }

        // Get balances and interest data for all users
        let (balances_raw, yt_interests_raw) = tokio::try_join!(
            self.multicall
                .get_all_erc20_balances(yt_token_address, all_yt_users, block_number),
            self.multicall
                .get_all_yt_interest_data(yt_token_address, all_yt_users, block_number)
        )?;

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

        if users_yt_index == U256::ZERO {
            return Ok(());
        }

        // Track YT balances for interest calculation
        let mut users_yt_balances: HashMap<Address, U256> = HashMap::new();

        // Get fee rate from factory
        let fee_rate = self
            .get_factory_fee_rate(general_data.factory, block_number)
            .await?;

        // Process balances with checked arithmetic. Entries that would
        // overflow or divide by zero are skipped instead of panicking, so a
        // single extreme balance cannot abort the whole snapshot.
        for (user, balance) in users_balances {
            let Some(implied_balance) = balance
                .checked_mul(WAD)
                .and_then(|v| v.checked_div(users_yt_index))
            else {
                continue;
            };
            let Some(fee_share) = implied_balance
                .checked_mul(fee_rate)
                .and_then(|v| v.checked_div(WAD))
            else {
                continue;
            };
            let remaining = implied_balance.saturating_sub(fee_share);

            self.increase_user_amount(yt_user_records, user, remaining);
            self.increase_user_amount(
                yt_user_records,
                *crate::constants::PENDLE_TREASURY,
                fee_share,
            );
            users_yt_balances.insert(user, balance);
        }

        // Process interests
        for (user, user_index, amount) in users_interests {
            if user == yt_token_address {
                continue;
            }
            if user_index == U256::ZERO {
                continue;
            }

            let user_balance = users_yt_balances.get(&user).copied().unwrap_or(U256::ZERO);
            if user_balance == U256::ZERO {
                continue;
            }

            // Checked interest math: skip entries that overflow or divide by zero.
            let Some(index_diff) = users_yt_index.checked_sub(user_index) else {
                continue;
            };
            let Some(pending_interest) = user_balance
                .checked_mul(index_diff)
                .and_then(|v| v.checked_mul(WAD))
                .and_then(|v| v.checked_div(users_yt_index))
                .and_then(|v| v.checked_div(user_index))
            else {
                continue;
            };

            let total_interest = pending_interest.saturating_add(amount);
            self.increase_user_amount(yt_user_records, user, total_interest);
        }

        Ok(())
    }

    async fn apply_lp_holder_shares(
        &self,
        result: &mut UserBalance,
        sy_token_address: Address,
        lp_token_address: Address,
        lp_info: &FullMarketInfo,
        block_number: u64,
    ) -> Result<()> {
        // Get total SY balance held by the LP token. Use the first entry when
        // present, otherwise zero, so an empty multicall response cannot panic.
        let total_sy = self
            .multicall
            .get_all_erc20_balances(sy_token_address, &[lp_token_address], block_number)
            .await?
            .into_iter()
            .next()
            .unwrap_or(U256::ZERO);

        // Get all active balances for LP holders
        let all_active_balances = self
            .multicall
            .get_all_market_active_balances(lp_token_address, &lp_info.lp_holders, block_number)
            .await?;

        // Calculate total active supply with saturating addition.
        let total_active_supply = all_active_balances
            .iter()
            .fold(U256::ZERO, |acc, balance| acc.saturating_add(*balance));

        if total_active_supply == U256::ZERO {
            return Ok(());
        }

        // Parse the WLP address once for direct Address comparison.
        let wlp_address = lp_info
            .wlp_info
            .as_ref()
            .and_then(|info| Address::from_str(&info.wlp).ok());

        // Process each LP holder
        for (i, holder) in lp_info.lp_holders.iter().enumerate() {
            // Skip holders missing from a short multicall response.
            let Some(active_balance) = all_active_balances.get(i).copied() else {
                continue;
            };

            // Calculate boosted SY balance (proportional share) with checked math.
            let Some(boosted_sy_balance) = active_balance
                .checked_mul(total_sy)
                .and_then(|v| v.checked_div(total_active_supply))
            else {
                continue;
            };

            // Check if this is a WLP holder via direct Address equality.
            if let Some(wlp) = wlp_address {
                if *holder == wlp {
                    self.apply_wlp_holder_shares(result, lp_info, block_number, boosted_sy_balance)
                        .await?;
                    continue;
                }
            }

            // Check if this is a liquid locker via direct Address equality.
            let ll_data = lp_info
                .liquid_locker_datas
                .iter()
                .find(|data| data.lp_holder == *holder);

            if let Some(liquid_locker_data) = ll_data {
                // Liquid locker - resolve shares
                let shares = self
                    .resolve_liquid_locker(
                        boosted_sy_balance,
                        liquid_locker_data,
                        block_number,
                    )
                    .await?;
                self.increase_user_amounts(result, &shares);
            } else {
                // Regular holder - add balance directly
                self.increase_user_amount(result, *holder, boosted_sy_balance);
            }
        }

        Ok(())
    }

    // Add these placeholder methods that need implementation:
    async fn apply_wlp_holder_shares(
        &self,
        result: &mut UserBalance,
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
        let wlp_holder_addresses: Vec<Address> = wlp_info.wlp_holders.iter().map(|h| *h).collect();

        // Get balances for all WLP holders
        let balances = self
            .multicall
            .get_all_erc20_balances(wlp_address, &wlp_holder_addresses, block_number)
            .await?;

        // Calculate total supply with saturating addition.
        let total_supply = balances.iter().fold(U256::ZERO, |acc, &b| acc.saturating_add(b));

        if total_supply == U256::ZERO {
            return Ok(());
        }

        // Calculate SY per one WLP token (scaled by WAD) with checked math.
        let Some(sy_per_one_wlp) = boosted_sy_balance
            .checked_mul(WAD)
            .and_then(|v| v.checked_div(total_supply))
        else {
            return Err(anyhow::anyhow!("WLP share math overflow"));
        };

        // Track MM shares by type
        let mut total_mm_shares: HashMap<MmType, U256> = HashMap::new();

        // Process each WLP holder with bounds-checked balance access.
        for (i, holder) in wlp_info.wlp_holders.iter().enumerate() {
            let Some(wlp_balance) = balances.get(i).copied() else {
                continue;
            };
            // Checked share math: skip entries that overflow instead of panicking.
            let Some(user_share) = wlp_balance
                .checked_mul(sy_per_one_wlp)
                .and_then(|v| v.checked_div(WAD))
            else {
                continue;
            };

            // Check if this holder is a money market
            let mm_type = get_mm_type(lp_info, holder);

            if let Some(mm_type) = mm_type {
                *total_mm_shares.entry(mm_type).or_insert(U256::ZERO) += user_share;
            } else {
                self.increase_user_amount(result, *holder, user_share);
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

    fn soft_check(&self, shares: &[UserTempShare], upperbound: U256) -> Result<()> {
        // Saturating accumulation prevents a malicious share list from
        // wrapping the total and bypassing the upper-bound check.
        let total = shares
            .iter()
            .fold(U256::ZERO, |acc, share| acc.saturating_add(share.share));

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

        // Calculate shares with checked arithmetic. Sequential iteration is
        // used (not Rayon) to avoid blocking Tokio async worker threads.
        let mut user_temp_shares = Vec::with_capacity(balances.len());
        for (i, &balance) in balances.iter().enumerate() {
            let Some(user) = users.get(i).copied() else {
                continue;
            };
            let Some(share) = balance
                .checked_mul(sy_per_one_wlp)
                .and_then(|v| v.checked_div(WAD))
            else {
                continue;
            };
            user_temp_shares.push(UserTempShare { user, share });
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

        // Calculate total receipt balance with saturating addition.
        let total_receipt_balance = balances.iter().fold(U256::ZERO, |acc, &b| acc.saturating_add(b));

        if total_receipt_balance == U256::ZERO {
            return Ok(vec![]);
        }

        // Calculate shares for each user with checked math (sequential, async-safe).
        let mut user_temp_shares = Vec::new();
        for (j, &receipt_balance) in balances.iter().enumerate() {
            if receipt_balance == U256::ZERO {
                continue;
            }
            let Some(user) = users.get(j).copied() else {
                continue;
            };
            let Some(user_share) = receipt_balance
                .checked_mul(boosted_sy_balance)
                .and_then(|v| v.checked_div(total_receipt_balance))
            else {
                continue;
            };
            user_temp_shares.push(UserTempShare {
                user,
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

        // Calculate shares with checked math (sequential, async-safe).
        let mut user_temp_shares = Vec::with_capacity(balances.len());
        for (i, &balance) in balances.iter().enumerate() {
            let Some(user) = users.get(i).copied() else {
                continue;
            };
            let Some(share) = balance
                .checked_mul(sy_per_one_wlp)
                .and_then(|v| v.checked_div(WAD))
                .and_then(|v| v.checked_div(U256::from(SILO_DECIMALS_OFFSET)))
            else {
                continue;
            };
            user_temp_shares.push(UserTempShare { user, share });
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

        // Build calls for each user's position. Invalid market IDs from the
        // untrusted API are skipped instead of panicking the whole batch.
        let mut calls: Vec<contracts::Multicall3::Call> = Vec::new();
        let mut valid_users: Vec<MorphoUserInstance> = Vec::new();
        for mui in morpho_users {
            let Ok(market_id) = FixedBytes::<32>::from_str(&mui.market_id) else {
                continue;
            };

            // Create call data for position(marketId, user)
            let call_data = morpho_instance
                .position(market_id, mui.user)
                .calldata()
                .to_vec();

            calls.push(Call {
                target: morpho_address,
                callData: call_data.into(),
            });
            valid_users.push(mui.clone());
        }

        // Execute multicall
        let results = self
            .multicall
            .try_aggregate_multicall(calls, block_number)
            .await?;

        // Process results and calculate shares with checked math.
        // Short returndata decodes as zero; overflow entries are skipped.
        let mut user_temp_shares = Vec::with_capacity(results.len());
        for (i, data) in results.iter().enumerate() {
            let collateral = if data.len() < 32 {
                U256::ZERO
            } else {
                // The position function returns (collateral, supplyShares, borrowShares).
                // Only the first 32 bytes (collateral) are needed here.
                U256::from_be_bytes::<32>(data[0..32].try_into().unwrap_or([0u8; 32]))
            };

            let Some(entry) = valid_users.get(i) else {
                continue;
            };
            let Some(share) = collateral
                .checked_mul(sy_per_one_wlp)
                .and_then(|v| v.checked_div(WAD))
            else {
                continue;
            };

            user_temp_shares.push(UserTempShare {
                user: entry.user,
                share,
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
        block_timestamp: u64,
    ) -> Result<UserBalance> {
        let mut result = UserBalance::new();

        // Get balances for all LP holders
        let balances = self
            .multicall
            .get_all_erc20_balances(lp_market, lp_holders, block_number)
            .await?;

        // Get LP to SY exchange rate
        let price = self
            .get_lp_to_sy_rate(lp_market, yt, block_number, block_timestamp)
            .await?;

        // Process each LP holder with bounds-checked balances and direct
        // Address comparison for liquid lockers.
        for (i, holder) in lp_holders.iter().enumerate() {
            let Some(balance) = balances.get(i).copied() else {
                continue;
            };

            // Find if this holder is a liquid locker
            let ll_data = liquid_locker_datas
                .iter()
                .find(|data| data.lp_holder == *holder);

            if let Some(liquid_locker_data) = ll_data {
                // Liquid locker - distribute to receipt token holders
                let users = liquid_locker_data.users.clone();

                // Convert user addresses
                let receipt_token = liquid_locker_data.receipt_token;
                let user_addresses = users.into_iter().collect::<Vec<_>>();

                // Get receipt token balances
                let receipt_balances = self
                    .multicall
                    .get_all_erc20_balances(receipt_token, &user_addresses, block_number)
                    .await?;

                // Calculate total receipt balance with saturating addition.
                let total_receipt_balance = receipt_balances
                    .iter()
                    .fold(U256::ZERO, |acc, &b| acc.saturating_add(b));

                if total_receipt_balance == U256::ZERO {
                    continue;
                }

                // Distribute to receipt token holders with checked math.
                for (j, user_addr) in user_addresses.iter().enumerate() {
                    let Some(receipt_balance) = receipt_balances.get(j).copied() else {
                        continue;
                    };

                    if receipt_balance == U256::ZERO {
                        continue;
                    }

                    // Calculate user's share; skip on overflow instead of panicking.
                    let Some(user_share) = receipt_balance
                        .checked_mul(balance)
                        .and_then(|v| v.checked_mul(price))
                        .and_then(|v| v.checked_div(total_receipt_balance))
                        .and_then(|v| v.checked_div(WAD))
                    else {
                        continue;
                    };

                    self.increase_user_amount(&mut result, *user_addr, user_share);
                }
            } else {
                // Regular holder - add their value directly with checked math.
                let Some(value) = balance
                    .checked_mul(price)
                    .and_then(|v| v.checked_div(WAD))
                else {
                    continue;
                };
                self.increase_user_amount(&mut result, *holder, value);
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
        block_timestamp: u64,
    ) -> Result<U256> {
        let market = PendleMarketInstance::new(lp_market, self.rpc_provider.clone());
        let yt_contract = PendleYieldTokenInstance::new(yt, self.rpc_provider.clone());

        // Read market state
        let state = market
            .readState(lp_market)
            .call()
            .block(block_number.into())
            .await?;

        // Calculate time to expiry (in seconds)
        let time_to_expiry = state.expiry.saturating_sub(U256::from(block_timestamp));

        // Calculate PT to asset rate. Failures are propagated instead of
        // silently using a zero rate that would corrupt downstream math.
        let time_to_expiry_u64: u64 = time_to_expiry.try_into().unwrap_or(0);
        let pt_to_asset_rate = self
            .get_exchange_rate_from_ln_implied_rate(state.lastLnImpliedRate, time_to_expiry_u64)?;

        // Get pyIndexCurrent
        let py_index = yt_contract
            .pyIndexCurrent()
            .call()
            .block(block_number.into())
            .await?;

        // Calculate PT to SY rate with checked math. A zero pyIndex is a
        // data error, not a valid divisor, so it returns an error.
        if py_index == U256::ZERO {
            return Err(anyhow::anyhow!("pyIndexCurrent is zero"));
        }
        let Some(pt_to_sy_rate) = pt_to_asset_rate
            .checked_mul(WAD)
            .and_then(|v| v.checked_div(py_index))
        else {
            return Err(anyhow::anyhow!("PT to SY rate overflow"));
        };

        // Calculate total value in SY with saturating/checked math.
        let total_sy = U256::try_from(state.totalSy).unwrap_or(U256::ZERO);
        let total_pt = U256::try_from(state.totalPt).unwrap_or(U256::ZERO);
        let pt_value_in_sy = total_pt
            .checked_mul(pt_to_sy_rate)
            .and_then(|v| v.checked_div(WAD))
            .unwrap_or(U256::ZERO);
        let total_value_in_sy = total_sy.saturating_add(pt_value_in_sy);

        // A zero totalLp would previously be masked as 1, producing an
        // astronomically large rate. Return an explicit error instead.
        let total_lp = U256::try_from(state.totalLp).unwrap_or(U256::ZERO);
        if total_lp.is_zero() {
            return Err(anyhow::anyhow!("totalLp is zero"));
        }
        let Some(rate) = total_value_in_sy
            .checked_mul(WAD)
            .and_then(|v| v.checked_div(total_lp))
        else {
            return Err(anyhow::anyhow!("LP to SY rate overflow"));
        };
        Ok(rate)
    }

    /// Convert a log implied rate to an `exp(rate * t)` scaling factor.
    /// Returns an error on non-finite inputs or overflow instead of silently
    /// returning zero or saturating to `u128::MAX`.
    fn get_exchange_rate_from_ln_implied_rate(
        &self,
        ln_implied_rate: U256,
        time_to_expiry: u64,
    ) -> Result<U256> {
        // Convert ln_implied_rate from U256 to f64 (assuming 18 decimals).
        let ln_rate_f64 = from_u256_to_decimal(ln_implied_rate)
            .map_err(|e| anyhow::anyhow!("Invalid implied rate: {e}"))?
            .to_f64()
            .ok_or_else(|| anyhow::anyhow!("Implied rate is not finite"))?;

        // Calculate normalized rate and reject non-finite intermediate values.
        let one_year = crate::constants::ONE_YEAR as f64;
        let normalized_rate = (ln_rate_f64 * time_to_expiry as f64) / one_year;
        if !normalized_rate.is_finite() {
            return Err(anyhow::anyhow!("Non-finite expiry math"));
        }

        // Calculate e^normalized_rate with overflow protection.
        let exp_rate = normalized_rate.exp();
        if !exp_rate.is_finite() || exp_rate < 0.0 {
            return Err(anyhow::anyhow!("Implied rate exponent overflow"));
        }
        let scaled = exp_rate.checked_mul(1e18).ok_or_else(|| {
            anyhow::anyhow!("Implied rate scaling overflow")
        })?;
        if scaled > u128::MAX as f64 {
            return Err(anyhow::anyhow!("Implied rate exceeds u128 range"));
        }

        // Convert back to U256 with 18 decimals.
        Ok(U256::from(scaled as u128))
    }

    /// Increase a user's amount in the result map with saturating addition
    /// so an extreme value cannot wrap around and corrupt balances.
    fn increase_user_amount(&self, result: &mut UserBalance, user: Address, amount: U256) {
        let entry = result.entry(user).or_insert(U256::ZERO);
        *entry = entry.saturating_add(amount);
    }

    /// Increase multiple users' amounts using UserTempShare data
    fn increase_user_amounts(&self, result: &mut UserBalance, datas: &[UserTempShare]) {
        for data in datas {
            self.increase_user_amount(result, data.user, data.share);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::Address;
    use dotenv::dotenv;
    use lazy_static::lazy_static;

    use crate::types::Market;

    use super::*;

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

    struct TestFixture {
        pool_config: PoolConfig,
        fetcher: PendleBalanceFetcher,
    }

    fn setup() -> TestFixture {
        // Report dotenv parse issues instead of silently ignoring them.
        if let Err(e) = dotenv() {
            eprintln!("dotenv load notice: {e:?}");
        }

        // Live-network tests require an explicit RPC endpoint.
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set for live tests");

        let pool_config = PoolConfig {
            sy: KHYPE_SY.clone(),
            yt: KHYPE_YT.clone(),
            lps: KHYPE_LPS.clone(),
        };

        let fetcher = PendleBalanceFetcher::builder()
            .rpc_url(rpc_url)
            .unwrap()
            .build()
            .unwrap();

        TestFixture {
            pool_config,
            fetcher,
        }
    }

    // Live-network integration test: requires RPC_URL and external APIs.
    #[tokio::test]
    #[ignore]
    async fn test_fetch_user_balance_snapshot_batch() {
        let fixture = setup();
        let blocks = [11474000, 11474100];

        let results = fixture
            .fetcher
            .fetch_user_balance_snapshot_batch(&fixture.pool_config, &blocks, PoolType::Shares)
            .await
            .unwrap();

        println!("result: {:?}", results);
    }

    #[tokio::test]
    #[ignore]
    async fn test_fetch_user_balance_snapshot_batch_for_user() {
        let fixture = setup();
        let user = Address::from_str("0x831e4ecf60dd9727d30c3610fa15ad52fa1ff54f").unwrap();
        let blocks = [13057329];

        let results = fixture
            .fetcher
            .fetch_user_balance_snapshot_batch(&fixture.pool_config, &blocks, PoolType::Shares)
            .await
            .unwrap();

        for result in results {
            let yt_user_record = result.yt_user_records_in_sy.get(&user);
            println!("yt_user_record: {:?}", yt_user_record);

            let lp_user_record = result.lp_user_records_in_sy.get(&user);
            println!("lp_user_record: {:?}", lp_user_record);

            let lp_user_record_underlying = result.lp_user_records_in_underlying.get(&user);
            println!("lp_user_record_underlying: {:?}", lp_user_record_underlying);

            let yt_user_record_underlying = result.yt_user_records_in_underlying.get(&user);
            println!("yt_user_record_underlying: {:?}", yt_user_record_underlying);
        }
    }
}
