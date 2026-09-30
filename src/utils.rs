use std::str::FromStr;

use alloy::primitives::{Address, U256};
use anyhow::Result;
use rust_decimal::Decimal;

use crate::{
    client::FullMarketInfo,
    types::{UserBalance, protocols::MmType},
};

const TOKEN_DECIMALS: u32 = 18;

/// Exact 1e18 scaling factor as an integer. Never construct this from an
/// `f64` literal (for example `U256::from(1e18)`), because float conversion
/// loses precision and corrupts share math.
pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

pub fn from_u256_to_decimal(value: U256) -> Result<Decimal> {
    let mut dec = Decimal::from_str(&value.to_string())?;
    dec.set_scale(TOKEN_DECIMALS)?;
    Ok(dec)
}

pub fn get_mm_type(lp_info: &FullMarketInfo, holder: &Address) -> Option<MmType> {
    lp_info
        .wlp_info
        .as_ref()?
        .remap_mm_holder
        .get(holder)
        .map(|mm_map| mm_map.mm_type.clone())
}

/// Convert SY balances to underlying balances using checked arithmetic.
/// Uses a sequential iterator (not Rayon) so calling this from async Tokio
/// tasks does not block the async worker threads. Overflow or division
/// failures yield a zero balance for that entry instead of panicking.
pub fn sy_balances_to_underlying(
    sy_user_balances: &UserBalance,
    exchange_rate: U256,
) -> UserBalance {
    sy_user_balances
        .iter()
        .filter_map(|(user, balance)| {
            let underlying_balance = balance
                .checked_mul(exchange_rate)?
                .checked_div(WAD)?;
            if underlying_balance > U256::ZERO {
                Some((user.to_owned(), underlying_balance))
            } else {
                None
            }
        })
        .collect()
}
