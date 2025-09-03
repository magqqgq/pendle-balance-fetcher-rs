use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EulerUserInstance {
    pub user: Address,
    pub sub_account: Address,
    pub asset: Address,
}
