use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router, Server,
};
use bitcoin::{Block, Transaction};
use futures::{sink::SinkExt, stream::StreamExt};
use kyoto::{builder::NodeBuilder, chain::checkpoints::HeaderCheckpoint};
use kyoto::{Event, IndexedBlock, Network};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use axum::extract::State;
use bitcoin::hex::{Case, DisplayHex};
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tracing::info;

// Shared state between Kyoto and Axum
#[derive(Debug)]
struct AppState {
    tx_sender: Sender<String>,
    block_sender: Sender<String>,
}

#[derive(Serialize, Deserialize)]
struct TxEvent {
    txid: String,
    hex: String,
}

#[derive(Serialize, Deserialize)]
struct BlockEvent {
    hash: String,
    height: u32,
    hex: String,
}

#[tokio::main]
async fn main() {
    // Set up logging
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // Create broadcast channels
    let (tx_sender, _) = broadcast::channel::<String>(1000);
    let (block_sender, _) = broadcast::channel::<String>(100);

    let shared_state = Arc::new(AppState {
        tx_sender: tx_sender.clone(),
        block_sender: block_sender.clone(),
    });

    // Start Kyoto Bitcoin node in background
    let state_clone = shared_state.clone();
    tokio::spawn(async move {
        run_bitcoin_node(state_clone).await;
    });

    // Set up Axum web server with WebSocket
    let app = Router::new()
        .route("/ws/transactions", get(ws_transactions_handler))
        .route("/ws/blocks", get(ws_blocks_handler))
        .with_state(shared_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    tracing::info!("WebSocket server running on {}", addr);

    Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

#[tracing::instrument(skip_all)]
async fn run_bitcoin_node(state: Arc<AppState>) {
    // Configure Bitcoin network (use Network::Bitcoin for mainnet)
    let network = Network::Bitcoin;

    // Set up Kyoto node
    let builder = NodeBuilder::new(network);
    let (node, client) = builder
        .required_peers(2)
        .build()
        .unwrap();

    // Get references to all receivers to monitor node activity
    let mut log_rx = client.log_rx;
    let mut info_rx = client.info_rx;
    let mut warn_rx = client.warn_rx;
    let mut event_rx = client.event_rx;

    // Run node in background
    tokio::spawn(async move {
        if let Err(e) = node.run().await {
            tracing::error!("Node error: {:?}", e);
        }
    });

    // Monitor node logs, info, and warnings
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(log) = log_rx.recv() => {
                    tracing::info!("Node log: {}", log);
                },
                Some(info) = info_rx.recv() => {
                    tracing::info!("Node info: {}", info);
                },
                Some(warn) = warn_rx.recv() => {
                    tracing::warn!("Node warning: {}", warn);
                },
            }
        }
    });

    // Listen for events from the Bitcoin network
    while let Some(event) = event_rx.recv().await {
        match event {
            Event::Block(indexed_block) => {
                tracing::info!("Received block at height: {}", indexed_block.height);
                process_block(indexed_block, &state.block_sender).await;
            },
            Event::Synced(update) => {
                tracing::info!("Node synced to height: {}", update.tip().height);
            },
            _ => {
                // Handle other events if needed
                tracing::debug!("Received other event type");
            }
        }
    }
}

#[tracing::instrument(skip_all)]
async fn process_block(indexed_block: IndexedBlock, sender: &Sender<String>) {
    let block_event = BlockEvent {
        hash: indexed_block.block.block_hash().to_string(),
        height: indexed_block.height,
        hex: bitcoin::consensus::serialize(&indexed_block.block).to_hex_string(Case::Lower),
    };

    // Process transactions within the block
    for tx in &indexed_block.block.txdata {
        process_transaction(tx, sender).await;
    }

    // Send block to websocket clients
    if let Ok(json) = serde_json::to_string(&block_event) {
        let _ = sender.send(json);
    }
}

#[tracing::instrument(skip_all)]
async fn process_transaction(tx: &Transaction, sender: &Sender<String>) {
    let txid =
        tx.compute_txid().to_string();
    info!("Processing transaction {txid}"); 
    let tx_event = TxEvent {
        txid: txid.to_string(),
        hex: bitcoin::consensus::serialize(tx).to_hex_string(Case::Lower),
    };

    if let Ok(json) = serde_json::to_string(&tx_event) {
        let _ = sender.send(json);
    }
}

// WebSocket handler for transactions
#[tracing::instrument(skip_all)]
async fn ws_transactions_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    info!("New WebSocket connection");
    ws.on_upgrade(|socket| handle_transaction_socket(socket, state))
}

// WebSocket handler for blocks
#[tracing::instrument]
#[tracing::instrument(skip_all)]
async fn ws_blocks_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_block_socket(socket, state))
}

#[tracing::instrument(skip_all)]
async fn handle_transaction_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to transaction broadcast channel
    let mut rx = state.tx_sender.subscribe();

    // Send transactions to the WebSocket client
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Receive WebSocket messages (can handle client requests or just keep-alive)
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(_)) = receiver.next().await {
            // Handle client messages if needed
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}

#[tracing::instrument]
#[tracing::instrument(skip_all)]
async fn handle_block_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to block broadcast channel
    let mut rx = state.block_sender.subscribe();

    // Send blocks to the WebSocket client
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Receive WebSocket messages
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(_)) = receiver.next().await {
            // Handle client messages if needed
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}