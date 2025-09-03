use alloy::primitives::{Address, BlockNumber, U256};
use indexmap::IndexMap;

pub mod protocols;
pub mod provider;

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub sy: Address,
    pub yt: Address,
    pub lps: Vec<Market>,
}

#[derive(Debug, Clone)]
pub struct Market {
    pub address: Address,
    pub deployed_block: BlockNumber,
}

#[derive(Debug, Clone, Copy)]
pub enum PoolType {
    Shares,
    LpValueInSy,
}

pub type UserRecord = IndexMap<Address, U256>;

#[derive(Debug, Default, Clone)]
pub struct SnapshotResult {
    pub block_number: BlockNumber,
    pub yt_user_records_in_sy: UserRecord,
    pub lp_user_records_in_sy: UserRecord,
    pub yt_user_records_in_underlying: UserRecord,
    pub lp_user_records_in_underlying: UserRecord,
}

#[derive(Debug, Clone)]
pub struct UserTempShare {
    pub user: Address,
    pub share: U256,
}

#[derive(Debug, Clone, Copy)]
pub struct YtInterestData {
    pub index: U256,
    pub accrue: U256,
}
