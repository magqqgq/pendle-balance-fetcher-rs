use serde::{Deserialize, Serialize};

pub mod euler;
pub mod liquid_locker;
pub mod morpho;
pub mod silo;

pub use euler::*;
pub use liquid_locker::*;
pub use morpho::*;
pub use silo::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MmType {
    Euler,
    Morpho,
    Silo,
}
