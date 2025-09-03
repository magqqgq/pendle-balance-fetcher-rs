use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiquidLockerData {
    pub name: String,
    pub lp_holder: Address,
    pub receipt_token: Address,
    pub users: Vec<Address>,
}
