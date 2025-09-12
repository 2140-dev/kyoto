//! Sync a simple script with the Bitcoin network. This example is intended to demonstrate the
//! expected sync time on your machine and in your region.

use bip157::builder::Builder;
use bip157::{lookup_host, Client, Event, HeaderCheckpoint, Network, ScriptBuf};
use std::collections::HashSet;
use std::net::Ipv4Addr;
use tokio::time::Instant;

const NETWORK: Network = Network::Bitcoin;
const HOST_ADDR: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

#[tokio::main]
async fn main() {
    // Add third-party logging
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let now = Instant::now();
    // Add Bitcoin scripts to scan the blockchain for
    let address = ScriptBuf::new_op_return(b"Kyoto light client");
    let mut addresses = HashSet::new();
    addresses.insert(address);
    let seeds = lookup_host("dnsseed.bitcoin.dashjr-list-of-p2p-nodes.us", HOST_ADDR).await;
    // Create a new node builder
    let builder = Builder::new(NETWORK);
    // Add node preferences and build the node/client
    let (node, client) = builder
        // The number of connections we would like to maintain
        .required_peers(2)
        // Only scan for taproot scripts
        .after_checkpoint(HeaderCheckpoint::taproot_activation())
        // Add some initial peers
        .add_peers(seeds.into_iter().map(From::from))
        // Create the node and client
        .build()
        .unwrap();
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
                if let Some(event) = event {
                    match event {
                        Event::FiltersSynced(update) => {
                            tracing::info!("Chain tip: {}",update.tip().hash);
                            // Request information from the node
                            let fee = requester.broadcast_min_feerate().await.unwrap();
                            tracing::info!("Minimum transaction broadcast fee rate: {:#}", fee);
                            let sync_time = now.elapsed().as_secs_f32();
                            tracing::info!("Total sync time: {sync_time} seconds");
                            let avg_fee_rate = requester.average_fee_rate(update.tip().hash).await.unwrap();
                            tracing::info!("Last block average fee rate: {:#}", avg_fee_rate);
                            break;
                        },
                        Event::IndexedFilter(_) => (),
                        _ => (),
                    }
                }
            }
            info = info_rx.recv() => {
                if let Some(info) = info {
                    tracing::info!("{info}");
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
