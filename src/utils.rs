use std::str::FromStr;

use alloy::primitives::U256;
use anyhow::Result;
use rayon::prelude::*;
use rust_decimal::Decimal;

use crate::types::UserBalance;

const TOKEN_DECIMALS: u32 = 18;

pub fn from_u256_to_decimal(value: U256) -> Result<Decimal> {
    let mut dec = Decimal::from_str(&value.to_string())?;
    dec.set_scale(TOKEN_DECIMALS)?;
    Ok(dec)
}

pub fn sy_balances_to_underlying(
    sy_user_balances: &UserBalance,
    exchange_rate: U256,
) -> UserBalance {
    let one = U256::from(1e18);
    sy_user_balances
        .par_iter()
        .map(|(user, balance)| {
            let underlying_balance = balance * exchange_rate / one;
            (user.to_owned(), underlying_balance)
        })
        .filter(|(_, balance)| balance > &U256::ZERO)
        .collect()
}
