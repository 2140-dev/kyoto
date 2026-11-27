use std::str::FromStr;

use bitcoin::{constants::genesis_block, params::Params, BlockHash};

type Height = u32;

/// A known block hash in the chain of most work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Checkpoint {
    /// The index of the block hash.
    height: Height,
    /// The Bitcoin block hash expected at this height
    hash: BlockHash,
}

impl Checkpoint {
    /// Create a new checkpoint from a known checkpoint of significant work.
    pub fn new(height: Height, hash: BlockHash) -> Self {
        Checkpoint { height, hash }
    }

    /// The index of the block hash.
    pub fn height(&self) -> Height {
        self.height
    }

    /// The Bitcoin block hash expected at this height
    pub fn block_hash(&self) -> BlockHash {
        self.hash
    }

    /// Build a checkpoint from the genesis block for a given network.
    pub fn from_genesis(params: impl AsRef<Params>) -> Self {
        let genesis_block = genesis_block(params);
        let hash = genesis_block.block_hash();
        Checkpoint { height: 0, hash }
    }

    /// One block before the activation of the taproot softfork.
    ///
    /// # Note
    ///
    /// This checkpoint is only available on the Bitcoin network (`main`, "mainnet").
    pub fn taproot_activation() -> Self {
        let hash = "000000000000000000013712fc242ee6dd28476d0e9c931c75f83e6974c6bccc"
            .parse::<BlockHash>()
            .unwrap();
        let height = 709_631;
        Checkpoint { height, hash }
    }

    /// One block before the activation of the segwit softfork.
    ///
    /// # Note
    ///
    /// This checkpoint is only available on the Bitcoin network (`main`, "mainnet").
    pub fn segwit_activation() -> Self {
        let hash = "000000000000000000cbeff0b533f8e1189cf09dfbebf57a8ebe349362811b80"
            .parse::<BlockHash>()
            .unwrap();
        let height = 481_823;
        Checkpoint { height, hash }
    }
}

impl From<(u32, BlockHash)> for Checkpoint {
    fn from(value: (u32, BlockHash)) -> Self {
        Checkpoint::new(value.0, value.1)
    }
}

impl TryFrom<(u32, String)> for Checkpoint {
    type Error = <BlockHash as FromStr>::Err;

    fn try_from(value: (u32, String)) -> Result<Self, Self::Error> {
        let hash = BlockHash::from_str(&value.1)?;
        Ok(Checkpoint::new(value.0, hash))
    }
}

impl TryFrom<(u32, &str)> for Checkpoint {
    type Error = <BlockHash as FromStr>::Err;

    fn try_from(value: (u32, &str)) -> Result<Self, Self::Error> {
        let hash = BlockHash::from_str(value.1)?;
        Ok(Checkpoint::new(value.0, hash))
    }
}
