use alloy::primitives::Address;
use std::str::FromStr;

lazy_static::lazy_static! {
    pub static ref PENDLE_TREASURY: Address = Address::from_str("0xc328dfcd2c8450e2487a91daa9b75629075b7a43").unwrap();
    pub static ref PENDLE_ORACLE: Address = Address::from_str("0x9a9fa8338dd5e5b2188006f1cd2ef26d921650c2").unwrap();
    pub static ref MULTICALL_ADDRESS: Address = Address::from_str("0xca11bde05977b3631167028862be2a173976ca11").unwrap();
}

pub const HYPEREVM_CHAIN_ID: u64 = 999;
pub const MULTICALL_BATCH_SIZE: usize = 500;
pub const ORACLE_INTERVAL: u64 = 15;
pub const ONE_YEAR: u64 = 86400 * 365;
