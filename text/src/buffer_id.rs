use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BufferId(u64);

impl BufferId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}
