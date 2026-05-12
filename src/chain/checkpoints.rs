use std::str::FromStr;

use bitcoin::{constants::genesis_block, params::Params, BlockHash};

type Height = u32;

/// A known block hash in the chain of most work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashCheckpoint {
    /// The index of the block hash.
    pub height: Height,
    /// The Bitcoin block hash expected at this height
    pub hash: BlockHash,
}

impl HashCheckpoint {
    /// Create a new checkpoint from a known checkpoint of significant work.
    pub fn new(height: Height, hash: BlockHash) -> Self {
        HashCheckpoint { height, hash }
    }

    /// Build a checkpoint from the genesis block for a given network.
    pub fn from_genesis(params: impl AsRef<Params>) -> Self {
        let genesis_block = genesis_block(params);
        let hash = genesis_block.block_hash();
        HashCheckpoint { height: 0, hash }
    }

    /// One block before the activation of the taproot softfork.
    pub fn taproot_activation() -> Self {
        let hash = "000000000000000000013712fc242ee6dd28476d0e9c931c75f83e6974c6bccc"
            .parse::<BlockHash>()
            .unwrap();
        let height = 709_631;
        HashCheckpoint { height, hash }
    }

    /// One block before the activation of the segwit softfork.
    pub fn segwit_activation() -> Self {
        let hash = "000000000000000000cbeff0b533f8e1189cf09dfbebf57a8ebe349362811b80"
            .parse::<BlockHash>()
            .unwrap();
        let height = 481_823;
        HashCheckpoint { height, hash }
    }
}

impl std::cmp::PartialOrd for HashCheckpoint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for HashCheckpoint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.height.cmp(&other.height)
    }
}

impl From<(u32, BlockHash)> for HashCheckpoint {
    fn from(value: (u32, BlockHash)) -> Self {
        HashCheckpoint::new(value.0, value.1)
    }
}

impl TryFrom<(u32, String)> for HashCheckpoint {
    type Error = <BlockHash as FromStr>::Err;

    fn try_from(value: (u32, String)) -> Result<Self, Self::Error> {
        let hash = BlockHash::from_str(&value.1)?;
        Ok(HashCheckpoint::new(value.0, hash))
    }
}

impl TryFrom<(u32, &str)> for HashCheckpoint {
    type Error = <BlockHash as FromStr>::Err;

    fn try_from(value: (u32, &str)) -> Result<Self, Self::Error> {
        let hash = BlockHash::from_str(value.1)?;
        Ok(HashCheckpoint::new(value.0, hash))
    }
}
