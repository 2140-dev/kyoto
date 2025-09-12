use crate::impl_sourceless_error;
use core::fmt::Display;
use std::fmt::Debug;

#[derive(Debug, PartialEq)]
pub(crate) enum HeaderSyncError {
    EmptyMessage,
    HeadersNotConnected,
    InvalidHeaderWork,
    InvalidHeaderTimes,
    InvalidCheckpoint,
    MiscalculatedDifficulty,
    InvalidBits,
    FloatingHeaders,
}

impl Display for HeaderSyncError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HeaderSyncError::EmptyMessage => write!(f, "empty headers message."),
            HeaderSyncError::HeadersNotConnected => {
                write!(f, "the headers received do not connect.")
            }
            HeaderSyncError::InvalidHeaderWork => {
                write!(f, "one or more headers does not match its own PoW target.")
            }
            HeaderSyncError::InvalidHeaderTimes => {
                write!(f, "one or more headers does not have a valid block time.")
            }
            HeaderSyncError::InvalidCheckpoint => {
                write!(f, "a checkpoint in the chain did not match.")
            }
            HeaderSyncError::MiscalculatedDifficulty => {
                write!(f, "a computed difficulty adjustment did not match.")
            }
            HeaderSyncError::FloatingHeaders => write!(
                f,
                "the peer sent us a chain that does not connect to any header of ours."
            ),
            HeaderSyncError::InvalidBits => write!(
                f,
                "the target work does not adhere to basic transition requirements."
            ),
        }
    }
}

impl_sourceless_error!(HeaderSyncError);

#[derive(Debug)]
pub enum CFHeaderSyncError {
    EmptyMessage,
    UnknownStophash,
    StopHashMismatch,
    UnrequestedStophash,
    PrevHeaderMismatch,
    HeaderChainIndexOverflow,
    UnexpectedCFHeaderMessage,
    StartHeightMisalignment,
}

impl core::fmt::Display for CFHeaderSyncError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CFHeaderSyncError::EmptyMessage => write!(f, "empty headers message."),
            CFHeaderSyncError::UnknownStophash => {
                write!(f, "a stop hash received was not found in our chain.")
            }
            CFHeaderSyncError::StopHashMismatch => {
                write!(f, "the requested and received stop hashes do not match.")
            }
            CFHeaderSyncError::UnrequestedStophash => {
                write!(f, "we did not request this stop hash.")
            }
            CFHeaderSyncError::PrevHeaderMismatch => write!(f, "previous filter header mismatch."),
            CFHeaderSyncError::HeaderChainIndexOverflow => write!(
                f,
                "indexed out of bounds on the header chain trying to find a block hash."
            ),
            CFHeaderSyncError::UnexpectedCFHeaderMessage => write!(
                f,
                "we already had a message from this peer staged in our queue."
            ),
            CFHeaderSyncError::StartHeightMisalignment => write!(
                f,
                "the size of the batch and the requested start height do not align"
            ),
        }
    }
}

impl_sourceless_error!(CFHeaderSyncError);

#[derive(Debug)]
pub enum CFilterSyncError {
    UnknownStophash,
    UnrequestedStophash,
    UnknownFilterHash,
    MisalignedFilterHash,
}

impl core::fmt::Display for CFilterSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CFilterSyncError::UnknownStophash => {
                write!(f, "a stop hash recevied was not found in our chain.")
            }
            CFilterSyncError::UnrequestedStophash => {
                write!(f, "we did not request this stop hash.")
            }
            CFilterSyncError::UnknownFilterHash => write!(
                f,
                "we could not find the filter hash corresponding to that stop hash."
            ),
            CFilterSyncError::MisalignedFilterHash => write!(
                f,
                "the filter hash from our header chain and this filter hash do not match."
            ),
        }
    }
}

impl_sourceless_error!(CFilterSyncError);

#[derive(Debug)]
pub(crate) enum BlockScanError {
    NoBlockHash,
    InvalidMerkleRoot,
}

impl Display for BlockScanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BlockScanError::NoBlockHash => {
                write!(f, "the block sent to us does not have a known hash.")
            }
            BlockScanError::InvalidMerkleRoot => {
                write!(f, "the block sent to us does not have a merkle root that matches its header commitment.")
            }
        }
    }
}

impl_sourceless_error!(BlockScanError);
