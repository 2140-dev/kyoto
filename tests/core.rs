use std::{
    net::{IpAddr, SocketAddrV4},
    path::PathBuf,
    time::Duration,
};

use bip157::{
    chain::{checkpoints::HeaderCheckpoint, BlockHeaderChanges, ChainState},
    client::Client,
    node::Node,
    Address, BlockHash, Event, Info, ServiceFlags, Transaction, TrustedPeer, Warning,
};
use bitcoin::{
    absolute,
    address::NetworkChecked,
    key::{
        rand::{rngs::StdRng, SeedableRng},
        Keypair, Secp256k1, TapTweak,
    },
    secp256k1::SecretKey,
    sighash::{Prevouts, SighashCache},
    Amount, KnownHrp, OutPoint, ScriptBuf, Sequence, TapSighashType, TxIn, TxOut, Witness,
};
use corepc_node::serde_json;
use corepc_node::{anyhow, exe_path};
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
    let tempdir = tempfile::TempDir::new()?;
    conf.tmpdir = Some(tempdir.path().to_owned());
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
    socket_addr: SocketAddrV4,
    tempdir_path: PathBuf,
    chain_state: ChainState,
) -> (Node, Client) {
    let host = (IpAddr::V4(*socket_addr.ip()), Some(socket_addr.port()));
    let mut trusted: TrustedPeer = host.into();
    trusted.set_services(ServiceFlags::P2P_V2);
    let builder = bip157::builder::Builder::new(bitcoin::Network::Regtest);
    let builder = builder.chain_state(chain_state);
    let (node, client) = builder.add_peer(host).data_dir(tempdir_path).build();
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
                if let Some(Event::FiltersSynced(update)) = event {
                    assert_eq!(update.tip().hash, *best);
                    println!("Correct sync");
                    break;
                };
            }
        }
    }
}

async fn print_logs(mut info_rx: Receiver<Info>, mut warn_rx: UnboundedReceiver<Warning>) {
    loop {
        tokio::select! {
            log = info_rx.recv() => {
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
    let tempdir = tempfile::TempDir::new().unwrap().path().to_owned();
    // Mine some blocks
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 2).await;
    let best = best_hash(rpc);
    let (node, client) = new_node(
        socket_addr,
        tempdir,
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
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
            bip157::messages::Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                accepted: _,
                reorganized: blocks,
            }) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            bip157::messages::Event::FiltersSynced(update) => {
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
    let tempdir = tempfile::TempDir::new().unwrap().path().to_owned();
    // Mine some blocks
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 2).await;
    let best = best_hash(rpc);
    let (node, client) = new_node(
        socket_addr,
        tempdir,
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
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
            bip157::messages::Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                accepted: _,
                reorganized: blocks,
            }) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            bip157::messages::Event::FiltersSynced(update) => {
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
    let tempdir = tempfile::TempDir::new().unwrap().path().to_owned();
    // Mine a lot of blocks
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 500, 15).await;
    let best = best_hash(rpc);
    let (node, client) = new_node(
        socket_addr,
        tempdir,
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    let _ = requester.broadcast_min_feerate().await.unwrap();
    assert!(requester.is_running());
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn stop_reorg_resync() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir: PathBuf = tempfile::TempDir::new().unwrap().path().to_owned();
    // Mine some blocks.
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 2).await;
    let best = best_hash(rpc);
    let (node, client) = new_node(
        socket_addr,
        tempdir.clone(),
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    // Reorganize the blocks
    let old_best = best;
    let old_height = num_blocks(rpc);
    invalidate_block(rpc, &best).await;
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Spin up the node on a cold start
    let (node, client) = new_node(
        socket_addr,
        tempdir.clone(),
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    // Make sure the reorganization is caught after a cold start
    while let Some(message) = channel.recv().await {
        match message {
            bip157::messages::Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                accepted: _,
                reorganized: blocks,
            }) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            bip157::messages::Event::FiltersSynced(update) => {
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
    let (node, client) = new_node(
        socket_addr,
        tempdir,
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn stop_reorg_two_resync() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir: PathBuf = tempfile::TempDir::new().unwrap().path().to_owned();
    // Mine some blocks.
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 10, 2).await;
    let best = best_hash(rpc);
    let (node, client) = new_node(
        socket_addr,
        tempdir.clone(),
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
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
    let (node, client) = new_node(
        socket_addr,
        tempdir.clone(),
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    while let Some(message) = channel.recv().await {
        match message {
            bip157::messages::Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                accepted: _,
                reorganized: blocks,
            }) => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks.last().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.last().unwrap().height);
            }
            bip157::messages::Event::FiltersSynced(update) => {
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
    let (node, client) = new_node(
        socket_addr,
        tempdir,
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn stop_reorg_start_on_orphan() {
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir: PathBuf = tempfile::TempDir::new().unwrap().path().to_owned();
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 17, 3).await;
    let best = best_hash(rpc);
    let (node, client) = new_node(
        socket_addr,
        tempdir.clone(),
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
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
        socket_addr,
        tempdir.clone(),
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    let mut headers = Vec::new();
    // Ensure SQL is able to catch the fork by loading in headers from the database
    while let Some(message) = channel.recv().await {
        match message {
            bip157::messages::Event::ChainUpdate(BlockHeaderChanges::Connected(header)) => {
                headers.push(header);
            }
            bip157::messages::Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                accepted: _,
                reorganized: blocks,
            }) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks.first().unwrap().header.block_hash(), old_best);
                assert_eq!(old_height as u32, blocks.first().unwrap().height);
            }
            bip157::messages::Event::FiltersSynced(update) => {
                println!("Done");
                assert_eq!(update.tip().hash, best);
                break;
            }
            _ => {}
        }
    }
    drop(handle);
    requester.shutdown().unwrap();
    let best = best_hash(rpc);
    let (node, client) = new_node(
        socket_addr,
        tempdir.clone(),
        ChainState::Snapshot(headers.clone()),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    let handle = tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    drop(handle);
    requester.shutdown().unwrap();
    mine_blocks(rpc, &miner, 2, 1).await;
    let best = best_hash(rpc);
    // Make sure the node does not have any corrupted headers
    let (node, client) = new_node(socket_addr, tempdir, ChainState::Snapshot(headers.clone()));
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx,
        warn_rx,
        event_rx: mut channel,
    } = client;
    tokio::task::spawn(async move { print_logs(info_rx, warn_rx).await });
    // The node properly syncs after persisting a reorg
    sync_assert(&best, &mut channel).await;
    requester.shutdown().unwrap();
    rpc.stop().unwrap();
}

#[tokio::test]
async fn tx_can_broadcast() {
    let amount_to_us = Amount::from_sat(100_000);
    let amount_to_op_return = Amount::from_sat(50_000);
    let (bitcoind, socket_addr) = start_bitcoind(true).unwrap();
    let rpc = &bitcoind.client;
    let tempdir = tempfile::TempDir::new().unwrap().path().to_owned();
    // Build a random address to send to. The Regtest wallet should not own this
    let mut rng = StdRng::seed_from_u64(10001);
    let secret = SecretKey::new(&mut rng);
    let secp = Secp256k1::new();
    let keypair = Keypair::from_secret_key(&secp, &secret);
    let (internal_key, _) = keypair.x_only_public_key();
    let send_to_this_address = Address::p2tr(&secp, internal_key, None, KnownHrp::Regtest);
    // Mine some blocks and send to this address
    println!("Mining blocks to an internal address...");
    let miner = rpc.new_address().unwrap();
    mine_blocks(rpc, &miner, 110, 10).await;
    let tx_info = rpc
        .send_to_address(&send_to_this_address, amount_to_us)
        .unwrap();
    // Find the vout that the address owns
    let txid = tx_info.txid().unwrap();
    println!("Sent to {send_to_this_address}");
    let tx_details = rpc.get_transaction(txid).unwrap().details;
    let (vout, amt) = tx_details
        .iter()
        .find(|detail| detail.address.eq(&send_to_this_address.to_string()))
        .map(|detail| (detail.vout, detail.amount))
        .unwrap();
    println!("{amt} Bitcoin locked at {vout} vout in {txid}");
    // Build a transaction that Regtest does not know about
    let bytes = b"Am I spam?";
    let op_return = ScriptBuf::new_op_return(bytes);
    let txout = TxOut {
        script_pubkey: op_return.clone(),
        value: amount_to_op_return,
    };
    let outpoint = OutPoint { txid, vout };
    let txin = TxIn {
        previous_output: outpoint,
        script_sig: ScriptBuf::default(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::default(),
    };
    let mut unsigned_tx = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![txin],
        output: vec![txout],
    };
    // Sign the transaction, taken from: https://github.com/rust-bitcoin/rust-bitcoin/blob/master/bitcoin/examples/sign-tx-taproot.rs
    let input_index = 0;
    let sighash_type = TapSighashType::Default;
    let prevout = TxOut {
        script_pubkey: send_to_this_address.script_pubkey(),
        value: Amount::from_btc(amt.abs()).unwrap(),
    };
    let prevouts = vec![prevout];
    let prevouts = Prevouts::All(&prevouts);
    let mut sighasher = SighashCache::new(&mut unsigned_tx);
    let sighash = sighasher
        .taproot_key_spend_signature_hash(input_index, &prevouts, sighash_type)
        .unwrap();
    let tweaked: bitcoin::key::TweakedKeypair = keypair.tap_tweak(&secp, None);
    let msg = bitcoin::secp256k1::Message::from(sighash);
    let signature = secp.sign_schnorr(&msg, &tweaked.to_keypair());
    let signature = bitcoin::taproot::Signature {
        signature,
        sighash_type,
    };
    *sighasher.witness_mut(input_index).unwrap() = Witness::p2tr_key_spend(&signature);
    let tx = sighasher.into_transaction().to_owned();
    println!("Signed transaction");
    // Build a node to broadcast the transaction
    let (node, client) = new_node(
        socket_addr,
        tempdir.clone(),
        ChainState::Checkpoint(HeaderCheckpoint::from_genesis(bitcoin::Network::Regtest)),
    );
    tokio::task::spawn(async move { node.run().await });
    let Client {
        requester,
        info_rx: _,
        warn_rx: _,
        event_rx: _,
    } = client;
    tokio::time::timeout(Duration::from_secs(60), requester.broadcast_tx(tx))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn dns_works() {
    let hostname = bip157::lookup_host("seed.bitcoin.sipa.be").await;
    assert!(!hostname.is_empty());
}
