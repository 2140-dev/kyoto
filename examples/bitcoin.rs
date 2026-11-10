//! Sync a simple script with the Bitcoin network. This example is intended to demonstrate the
//! expected sync time on your machine and in your region.

use bip157::builder::Builder;
use bip157::chain::ChainState;
use bip157::{Client, Event, HashCheckpoint, Info, Network, ScriptBuf};
use std::collections::HashSet;
use tokio::time::Instant;

const NETWORK: Network = Network::Bitcoin;

#[tokio::main]
async fn main() {
    // Add third-party logging
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let now = Instant::now();
    let checkpoint = HashCheckpoint::taproot_activation();
    let start_height = checkpoint.height;
    let mut chain_height = start_height;
    // Add Bitcoin scripts to scan the blockchain for
    let address = ScriptBuf::new_op_return(b"Kyoto light client");
    let mut addresses = HashSet::new();
    addresses.insert(address);
    // Query DNS seeds for faster peer discovery
    // let seeds = bip157::lookup_host("seed.rustaceanrob.com").await;
    // Create a new node builder
    let builder = Builder::new(NETWORK);
    // Add node preferences and build the node/client
    let (node, client) = builder
        // The number of connections we would like to maintain
        .required_peers(2)
        // Only scan for taproot scripts
        .chain_state(ChainState::Checkpoint(checkpoint))
        // .headers_only_sync()
        // Add some initial peers
        // .add_peers(seeds.into_iter().map(From::from))
        // Connections over Tor are supported by Socks5 proxy
        // .socks5_proxy(bip157::Socks5Proxy::local())
        // Optionally request witness data when fetching a block
        // .fetch_witness_data()
        // Create the node and client
        .build();
    // Run the node on a separate task
    tokio::task::spawn(async move { node.run().await });
    // Split the client into components that send messages and listen to messages.
    // With this construction, different parts of the program can take ownership of
    // specific tasks.
    let Client {
        requester,
        mut info_rx,
        mut warn_rx,
        mut event_rx,
    } = client;
    // Continually listen for events until the node is synced to its peers.
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                if let Some(Event::FiltersSynced(update)) = event {
                    tracing::info!("Chain tip: {}",update.tip().hash);
                    // Request information from the node
                    let fee = requester.broadcast_min_feerate().await.unwrap();
                    tracing::info!("Minimum transaction broadcast fee rate: {:#}", fee);
                    let sync_time = now.elapsed().as_secs_f32();
                    tracing::info!("Total sync time: {sync_time} seconds");
                    let avg_fee_rate = requester.average_fee_rate(update.tip().hash).await.unwrap();
                    tracing::info!("Last block average fee rate: {:#}", avg_fee_rate);
                    break;
                }
            }
            info = info_rx.recv() => {
                match info {
                    Some(Info::NewChainHeight(height)) => {
                        chain_height = height;
                        tracing::info!("Header height: {height}");
                    }
                    Some(Info::CFHeadersAdded(height)) => {
                        let percent = percent_complete(start_height, height, chain_height);
                        tracing::info!("Filter headers: {percent:.2}% ({height}/{chain_height})");
                    }
                    Some(Info::FilterAdded(height)) if height % 2_000 == 0 => {
                        let percent = percent_complete(start_height, height, chain_height);
                        tracing::info!("Filters: {percent:.2}% ({height}/{chain_height})");
                    }
                    Some(Info::FilterAdded(_)) => (),
                    Some(info) => tracing::info!("{info}"),
                    None => (),
                }
            }
            warn = warn_rx.recv() => {
                if let Some(warn) = warn {
                    tracing::warn!("{warn}");
                }
            }
        }
    }
    let _ = requester.shutdown();
    tracing::info!("Shutting down");
}

fn percent_complete(start: u32, height: u32, tip: u32) -> f32 {
    let total = tip.saturating_sub(start).max(1);
    let done = height.saturating_sub(start).min(total);
    done as f32 / total as f32 * 100.0
}
