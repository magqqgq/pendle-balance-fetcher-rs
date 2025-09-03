use std::str::FromStr;

use alloy::primitives::U256;
use anyhow::Result;
use rust_decimal::Decimal;

const TOKEN_DECIMALS: u32 = 18;

pub fn from_u256_to_decimal(value: U256) -> Result<Decimal> {
    let mut dec = Decimal::from_str(&value.to_string())?;
    dec.set_scale(TOKEN_DECIMALS)?;
    Ok(dec)
}
