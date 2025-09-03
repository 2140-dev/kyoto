use std::fmt::{Debug, Display};

use crate::prelude::FutureResult;

use super::PersistedPeer;

/// Methods that define a list of peers on the Bitcoin P2P network.
pub trait PeerStore: Debug + Send + Sync {
    /// Errors that may occur within a [`PeerStore`].
    type Error: Debug + Display;
    /// Add a peer to the database, defining if it should be replaced or not.
    fn update(&mut self, peer: PersistedPeer) -> FutureResult<'_, (), Self::Error>;

    /// Get any peer from the database, selected at random. If no peers exist, an error is thrown.
    fn random(&mut self) -> FutureResult<'_, PersistedPeer, Self::Error>;

    /// The number of peers in the database that are not marked as banned.
    fn num_unbanned(&mut self) -> FutureResult<'_, u32, Self::Error>;
}

#[cfg(test)]
mod test {
    use super::*;

    /// Errors for the [`PeerStore`](crate) of unit type.
    #[derive(Debug)]
    pub enum UnitPeerStoreError {
        /// There were no peers found.
        NoPeers,
    }

    impl core::fmt::Display for UnitPeerStoreError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                UnitPeerStoreError::NoPeers => write!(f, "no peers in unit database."),
            }
        }
    }

    impl PeerStore for () {
        type Error = UnitPeerStoreError;
        fn update(&mut self, _peer: PersistedPeer) -> FutureResult<'_, (), Self::Error> {
            async fn do_update() -> Result<(), UnitPeerStoreError> {
                Ok(())
            }
            Box::pin(do_update())
        }

        fn random(&mut self) -> FutureResult<'_, PersistedPeer, Self::Error> {
            async fn do_random() -> Result<PersistedPeer, UnitPeerStoreError> {
                Err(UnitPeerStoreError::NoPeers)
            }
            Box::pin(do_random())
        }

        fn num_unbanned(&mut self) -> FutureResult<'_, u32, Self::Error> {
            async fn do_num_unbanned() -> Result<u32, UnitPeerStoreError> {
                Ok(0)
            }
            Box::pin(do_num_unbanned())
        }
    }
}
