use std::str::FromStr;

use bitcoin::BlockHash;

type Height = u32;

/// A known block hash in the chain of most work.
#[derive(Debug, Clone, Copy)]
pub struct HeaderCheckpoint {
    /// The index of the block hash.
    pub height: Height,
    /// The Bitcoin block hash expected at this height
    pub hash: BlockHash,
}

impl HeaderCheckpoint {
    /// Create a new checkpoint from a known checkpoint of significant work.
    pub fn new(height: Height, hash: BlockHash) -> Self {
        HeaderCheckpoint { height, hash }
    }

    /// One block before the activation of the taproot softfork.
    pub fn taproot_activation() -> Self {
        let hash = "000000000000000000013712fc242ee6dd28476d0e9c931c75f83e6974c6bccc"
            .parse::<BlockHash>()
            .unwrap();
        let height = 709_631;
        HeaderCheckpoint { height, hash }
    }

    /// One block before the activation of the segwit softfork.
    pub fn segwit_activation() -> Self {
        let hash = "000000000000000000cbeff0b533f8e1189cf09dfbebf57a8ebe349362811b80"
            .parse::<BlockHash>()
            .unwrap();
        let height = 481_823;
        HeaderCheckpoint { height, hash }
    }
}

impl From<(u32, BlockHash)> for HeaderCheckpoint {
    fn from(value: (u32, BlockHash)) -> Self {
        HeaderCheckpoint::new(value.0, value.1)
    }
}

impl TryFrom<(u32, String)> for HeaderCheckpoint {
    type Error = <BlockHash as FromStr>::Err;

    fn try_from(value: (u32, String)) -> Result<Self, Self::Error> {
        let hash = BlockHash::from_str(&value.1)?;
        Ok(HeaderCheckpoint::new(value.0, hash))
    }
}

impl TryFrom<(u32, &str)> for HeaderCheckpoint {
    type Error = <BlockHash as FromStr>::Err;

    fn try_from(value: (u32, &str)) -> Result<Self, Self::Error> {
        let hash = BlockHash::from_str(value.1)?;
        Ok(HeaderCheckpoint::new(value.0, hash))
    }
}
