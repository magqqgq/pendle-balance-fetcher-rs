use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiloUserInstance {
    pub user: Address,
    pub asset: Address,
}
