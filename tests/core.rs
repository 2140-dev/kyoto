use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

use bitcoin::{address::NetworkChecked, ScriptBuf};
use corepc_node::serde_json;
use corepc_node::{anyhow, exe_path};
use kyoto::{
    chain::checkpoints::HeaderCheckpoint, client::Client, node::Node, BlockHash, Event, LogLevel,
    ServiceFlags, SqliteHeaderDb, SqlitePeerDb, TrustedPeer, Warning,
};
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::UnboundedReceiver;

// Start the bitcoin daemon either through an environment variable or by download
fn start_bitcoind(with_v2_transport: bool) -> anyhow::Result<(corepc_node::Node, SocketAddrV4)> {
    let path = exe_path()?;
    let mut conf = corepc_node::Conf::default();
    conf.p2p = corepc_node::P2P::Yes;
    conf.args.push("--txindex");
    conf.args.push("--blockfilterindex");
    conf.args.push("--peerblockfilters");
    conf.args.push("--rest=1");
    conf.args.push("--server=1");
    conf.args.push("--listen=1");
    conf.tmpdir = Some(tempfile::TempDir::new().unwrap().into_path());
    if with_v2_transport {
        conf.args.push("--v2transport=1")
    } else {
        conf.args.push("--v2transport=0");
    }
    let bitcoind = corepc_node::Node::with_conf(path, &conf)?;
    let socket_addr = bitcoind.params.p2p_socket.unwrap();
    Ok((bitcoind, socket_addr))
}

fn new_node(
    addrs: HashSet<ScriptBuf>,
    socket_addr: SocketAddrV4,
    tempdir_path: PathBuf,
    checkpoint: Option<HeaderCheckpoint>,
) -> (Node<SqliteHeaderDb, SqlitePeerDb>, Client) {
    let host = (IpAddr::V4(*socket_addr.ip()), Some(socket_addr.port()));
    let mut trusted: TrustedPeer = host.into();
    trusted.set_services(ServiceFlags::P2P_V2);
    let mut builder = kyoto::builder::NodeBuilder::new(bitcoin::Network::Regtest);
    if let Some(checkpoint) = checkpoint {
        builder = builder.after_checkpoint(checkpoint);
    }
    let (node, client) = builder
        .add_peer(host)
        .add_scripts(addrs)
        .data_dir(tempdir_path)
        .build()
        .unwrap();
    (node, client)
}

fn num_blocks(rpc: &corepc_node::Client) -> i64 {
    rpc.get_blockchain_info().unwrap().blocks
}

fn best_hash(rpc: &corepc_node::Client) -> BlockHash {
    rpc.get_best_block_hash().unwrap().block_hash().unwrap()
}

async fn mine_blocks(
    rpc: &corepc_node::Client,
    miner: &bitcoin::Address<NetworkChecked>,
    num_blocks: usize,
    time: u64,
) {
    rpc.generate_to_address(num_blocks, miner).unwrap();
    tokio::time::sleep(Duration::from_secs(time)).await;
}

async fn invalidate_block(rpc: &corepc_node::Client, hash: &bitcoin::BlockHash) {
    let value = serde_json::to_value(hash).unwrap();
    rpc.call::<()>("invalidateblock", &[value]).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
}

async fn sync_assert(best: &bitcoin::BlockHash, channel: &mut UnboundedReceiver<Event>) {
    loop {
        tokio::select! {
            event = channel.recv() => {
                if let Some(Event::Synced(update)) = event {
                    assert_eq!(update.tip().hash, *best);
                    println!("Correct sync");
                    break;
                };
            }
        }
    }
}

async fn print_logs(mut log_rx: Receiver<String>, mut warn_rx: UnboundedReceiver<Warning>) {
    loop {
        tokio::select! {
            log = log_rx.recv() => {
                if let Some(log) = log {
                    println!("{log}")
                }
            }
            warn = warn_rx.recv() => {
                if let Some(warn) = warn {
                    println!("{warn}")
                }
            }
        }
    }
}

#[tokio::test]
async fn live_reorg() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir = tempfile::TempDir::new().unwrap().into_path();
    // Mine some blocks
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 1).await;
    let best = best_hash(rpc);
    // Build and run a node
    let mut scripts = HashSet::new();
    let other = rpc.new_address().unwrap();
    scripts.insert(other.into());
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir, None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    // Reorganize the blocks
    let old_best = best;
    let old_height = num_blocks(rpc);
    invalidate_block(rpc, &best).await;
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Make sure the reorg was caught
    while let Some(message) = channel.recv().await {
        match message {
            kyoto::messages::Event::BlocksDisconnected(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            kyoto::messages::Event::Synced(update) => {
                assert_eq!(update.tip().hash, best);
                requester.shutdown().unwrap();
                break;
            }
            _ => {}
        }
    }
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn live_reorg_additional_sync() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir = tempfile::TempDir::new().unwrap().into_path();
    // Mine some blocks
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 1).await;
    let best = best_hash(rpc);
    // Build and run a node
    let mut scripts = HashSet::new();
    let other = rpc.new_address().unwrap();
    scripts.insert(other.into());
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir, None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    // Reorganize the blocks
    let old_best = best;
    let old_height = num_blocks(rpc);
    let fetched_header = requester.get_header(10).await.unwrap();
    assert_eq!(old_best, fetched_header.block_hash());
    invalidate_block(rpc, &best).await;
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Make sure the reorg was caught
    while let Some(message) = channel.recv().await {
        match message {
            kyoto::messages::Event::BlocksDisconnected(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            kyoto::messages::Event::Synced(update) => {
                assert_eq!(update.tip().hash, best);
                break;
            }
            _ => {}
        }
    }
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn various_client_methods() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir = tempfile::TempDir::new().unwrap().into_path();
    // Mine a lot of blocks
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 500, 15).await;
    let best = best_hash(rpc);
    let mut scripts = HashSet::new();
    let other = rpc.new_address().unwrap();
    scripts.insert(other.into());
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir, None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    let batch = requester.get_header_range(10_000..10_002).await.unwrap();
    assert!(batch.is_empty());
    let _ = requester.broadcast_min_feerate().await.unwrap();
    let _ = requester.get_header(3).await.unwrap();
    let script = rpc.new_address().unwrap();
    requester.add_script(script).unwrap();
    assert!(requester.is_running());
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn stop_reorg_resync() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir = tempfile::TempDir::new().unwrap().into_path();
    // Mine some blocks.
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 1).await;
    let best = best_hash(rpc);
    let mut scripts = HashSet::new();
    let other = rpc.new_address().unwrap();
    scripts.insert(other.into());
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir.clone(), None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    let batch = requester.get_header_range(0..10).await.unwrap();
    assert!(!batch.is_empty());
    requester.shutdown().unwrap();
    // Reorganize the blocks
    let old_best = best;
    let old_height = num_blocks(rpc);
    invalidate_block(rpc, &best).await;
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Spin up the node on a cold start
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir.clone(), None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    // Make sure the reorganization is caught after a cold start
    while let Some(message) = channel.recv().await {
        match message {
            kyoto::messages::Event::BlocksDisconnected(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            kyoto::messages::Event::Synced(update) => {
                println!("Done");
                assert_eq!(update.tip().hash, best);
                break;
            }
            _ => {}
        }
    }
    requester.shutdown().unwrap();
    drop(handle);
    // Mine more blocks
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Make sure the node does not have any corrupted headers
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir, None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn stop_reorg_two_resync() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir = tempfile::TempDir::new().unwrap().into_path();
    // Mine some blocks.
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 1).await;
    let best = best_hash(rpc);
    let mut scripts = HashSet::new();
    let other = rpc.new_address().unwrap();
    scripts.insert(other.into());
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir.clone(), None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    // Reorganize the blocks
    let old_height = num_blocks(rpc);
    let old_best = best;
    invalidate_block(rpc, &best).await;
    let best = best_hash(rpc);
    invalidate_block(rpc, &best).await;
    mine_blocks(rpc, &miner, 3, 1).await;
    let best = best_hash(rpc);
    drop(handle);
    // Make sure the reorganization is caught after a cold start
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir.clone(), None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    while let Some(message) = channel.recv().await {
        match message {
            kyoto::messages::Event::BlocksDisconnected(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks.last().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.last().unwrap().height);
            }
            kyoto::messages::Event::Synced(update) => {
                println!("Done");
                assert_eq!(update.tip().hash, best);
                break;
            }
            _ => {}
        }
    }
    drop(handle);
    requester.shutdown().unwrap();
    // Mine more blocks
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Make sure the node does not have any corrupted headers
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir, None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn stop_reorg_start_on_orphan() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir = tempfile::TempDir::new().unwrap().into_path();
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 17, 3).await;
    let best = best_hash(rpc);
    let mut scripts = HashSet::new();
    let other = rpc.new_address().unwrap();
    scripts.insert(other.into());
    let (node, client) = new_node(scripts.clone(), socket_addr, tempdir.clone(), None);
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    drop(handle);
    requester.shutdown().unwrap();
    // Reorganize the blocks
    let old_best = best;
    let old_height = num_blocks(rpc);
    invalidate_block(rpc, &best).await;
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Spin up the node on a cold start with a stale tip
    let (node, client) = new_node(
        scripts.clone(),
        socket_addr,
        tempdir.clone(),
        Some(HeaderCheckpoint::new(old_height as u32, old_best)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    // Ensure SQL is able to catch the fork by loading in headers from the database
    while let Some(message) = channel.recv().await {
        match message {
            kyoto::messages::Event::BlocksDisconnected(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            kyoto::messages::Event::Synced(update) => {
                println!("Done");
                assert_eq!(update.tip().hash, best);
                break;
            }
            _ => {}
        }
    }
    drop(handle);
    requester.shutdown().unwrap();
    // Don't do anything, but reload the node from the checkpoint
    let cp = best_hash(rpc);
    let old_height = num_blocks(rpc);
    let best = best_hash(rpc);
    // Make sure the node does not have any corrupted headers
    let (node, client) = new_node(
        scripts.clone(),
        socket_addr,
        tempdir.clone(),
        Some(HeaderCheckpoint::new(old_height as u32, cp)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    drop(handle);
    requester.shutdown().unwrap();
    // Mine more blocks and reload from the checkpoint
    let cp = best_hash(rpc);
    let old_height = num_blocks(rpc);
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Make sure the node does not have any corrupted headers
    let (node, client) = new_node(
        scripts.clone(),
        socket_addr,
        tempdir,
        Some(HeaderCheckpoint::new(old_height as u32, cp)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        log_rx,
        info_rx: _,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(log_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn signet_syncs() {
    let tempdir = tempfile::TempDir::new().unwrap().into_path();
    let address = bitcoin::Address::from_str("tb1q9pvjqz5u5sdgpatg3wn0ce438u5cyv85lly0pc")
        .unwrap()
        .require_network(bitcoin::Network::Signet)
        .unwrap()
        .into();
    let mut set = HashSet::new();
    set.insert(address);
    let host = (IpAddr::from(Ipv4Addr::new(136, 41, 161, 69)), None);
    let builder = kyoto::builder::NodeBuilder::new(bitcoin::Network::Signet);
    let (node, client) = builder
        .add_peer(host)
        .add_scripts(set)
        .data_dir(tempdir)
        .log_level(LogLevel::Info)
        .build()
        .unwrap();
    let start_time = std::time::Instant::now();
    tokio::task::spawn(async move { node.run().await });
    async fn print_and_sync(mut client: Client) {
        loop {
            tokio::select! {
                event = client.event_rx.recv() => {
                    if let Some(Event::Synced(update)) = event {
                        println!("Synced chain up to block {}", update.tip().height);
                        println!("Chain tip: {}", update.tip().hash);
                        break;
                    }
                }
                log = client.info_rx.recv() => {
                    if let Some(log) = log {
                        println!("{log}");
                    }
                }
                warn = client.warn_rx.recv() => {
                    if let Some(warn) = warn {
                        println!("{warn}")
                    }
                }
            }
        }
    }
    let timeout = tokio::time::timeout(Duration::from_secs(180), print_and_sync(client)).await;
    assert!(timeout.is_ok());
    let elapsed = start_time.elapsed();
    println!("Signet sync completed in {elapsed:.2?}");
}
