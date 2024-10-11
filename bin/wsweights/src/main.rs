use futures_util::{StreamExt, SinkExt};
use serde::Serialize;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message};
use tokio_stream::wrappers::TcpListenerStream;
use subxt::{blocks::Block, OnlineClient, PolkadotConfig};
use shared::{registry::registered_paras, round_to};
use types::{Parachain, WeightConsumption, Timestamp};

const LOG_TARGET: &str = "tracker";

// Data structures for consumption updates sent over websocket
#[derive(Serialize)]
struct ConsumptionUpdate {
    para_id: u32,
    ref_time: RefTime,
    proof_size: ProofSize,
    total_proof_size: f32,
}

#[derive(Serialize)]
struct RefTime {
    normal: f32,
    operational: f32,
    mandatory: f32,
}

#[derive(Serialize)]
struct ProofSize {
    normal: f32,
    operational: f32,
    mandatory: f32,
}

#[subxt::subxt(runtime_metadata_path = "../../artifacts/metadata.scale")]
mod polkadot {}

// Type alias for tracking connected websocket clients
type ClientList = Arc<RwLock<Vec<Arc<RwLock<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>>>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder().filter_level(log::LevelFilter::Debug).init();

    let clients: ClientList = Arc::new(RwLock::new(Vec::new()));

    // Start websocket server for client connections
    let clients_for_ws = clients.clone();
    tokio::spawn(async move {
        start_websocket_server(clients_for_ws).await;
    });

    // Start tracking parachain data and send updates to websocket clients
    start_tracking(0, clients.clone()).await?;

    Ok(())
}

// Start websocket server to accept client connections
async fn start_websocket_server(clients: ClientList) {
    let addr = "127.0.0.1:9001".to_string();
    let listener = TcpListener::bind(&addr).await.expect("Can't bind to address");
    let stream = TcpListenerStream::new(listener);
    log::debug!("WebSocket server started at {}", addr);

    stream
        .for_each(|stream| {
            let clients = clients.clone();
            async move {
                if let Ok(stream) = stream {
                    let ws_stream = accept_async(stream).await.expect("Error during WebSocket handshake");
                    log::debug!("New client connected from {:?}", ws_stream.get_ref().peer_addr());
                    let ws_stream = Arc::new(RwLock::new(ws_stream));
                    clients.write().await.push(ws_stream);
                } else {
                    log::error!("Failed to accept new websocket connection");
                }
            }
        })
        .await;
}

// Handle each new websocket connection (already handled in start_websocket_server)

// Start tracking parachain consumption and send updates to websocket clients
async fn start_tracking(rpc_index: usize, clients: ClientList) -> Result<(), Box<dyn std::error::Error>> {
    let tasks: Vec<_> = registered_paras()
        .into_iter()
        .map(|para| {
            let clients_clone = clients.clone();
            tokio::spawn(async move {
                track_weight_consumption(para, rpc_index, clients_clone).await
            })
        })
        .collect();

    for task in tasks {
        task.await.expect("Failed to track consumption");
    }

    Ok(())
}

// Track weight consumption for each parachain and broadcast updates to websocket clients
async fn track_weight_consumption(para: Parachain, rpc_index: usize, clients: ClientList) {
    let Some(rpc) = para.rpcs.get(rpc_index) else {
        log::error!(
            target: LOG_TARGET,
            "{}-{} - doesn't have an rpc with index: {}",
            para.relay_chain, para.para_id, rpc_index,
        );
        return;
    };

    log::info!("{}-{} - Starting to track consumption.", para.relay_chain, para.para_id);
    let result = OnlineClient::<PolkadotConfig>::from_url(rpc).await;

    if let Ok(api) = result {
        if let Err(err) = track_blocks(api, para.clone(), rpc_index, clients).await {
            log::error!(
                target: LOG_TARGET,
                "{}-{} - Failed to track new block: {:?}",
                para.relay_chain,
                para.para_id,
                err
            );
        }
    } else {
        log::error!(
            target: LOG_TARGET,
            "{}-{} - Failed to create online client: {:?}",
            para.relay_chain,
            para.para_id,
            result
        );
    }
}

async fn track_blocks(
    api: OnlineClient<PolkadotConfig>,
    para: Parachain,
    rpc_index: usize,
    clients: ClientList,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!(
        target: LOG_TARGET,
        "{}-{} - Subscribing to finalized blocks",
        para.relay_chain,
        para.para_id
    );

    let mut blocks_sub = api
        .blocks()
        .subscribe_finalized()
        .await
        .map_err(|_| "Failed to subscribe to finalized blocks")?;

    while let Some(Ok(block)) = blocks_sub.next().await {
        note_new_block(api.clone(), para.clone(), rpc_index, block, clients.clone()).await?;
    }

    Ok(())
}

// Collect consumption data for each finalized block and broadcast to websocket clients
async fn note_new_block(
    api: OnlineClient<PolkadotConfig>,
    para: Parachain,
    rpc_index: usize,
    block: Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    clients: ClientList,
) -> Result<(), Box<dyn std::error::Error>> {
    let block_number = block.header().number;
    let timestamp = timestamp_at(api.clone(), block.hash()).await?;
    let consumption = weight_consumption(api, block_number, timestamp).await?;

    let consumption_update = ConsumptionUpdate {
        para_id: para.para_id,
        ref_time: RefTime {
            normal: consumption.ref_time.normal,
            operational: consumption.ref_time.operational,
            mandatory: consumption.ref_time.mandatory,
        },
        proof_size: ProofSize {
            normal: consumption.proof_size.normal,
            operational: consumption.proof_size.operational,
            mandatory: consumption.proof_size.mandatory,
        },
        total_proof_size: consumption.total_proof_size,
    };

    let consumption_json = serde_json::to_string(&consumption_update)?;

    // Broadcast the consumption update to all connected websocket clients
    let clients = clients.read().await;
    for client in clients.iter() {
        let mut ws_stream = client.write().await;
        ws_stream.send(Message::Text(consumption_json.clone())).await?;
    }

    Ok(())
}

// Fetch weight consumption data for the given block
async fn weight_consumption(
    api: OnlineClient<PolkadotConfig>,
    block_number: u32,
    timestamp: Timestamp,
) -> Result<WeightConsumption, Box<dyn std::error::Error>> {
    let weight_query = polkadot::storage().system().block_weight();
    let weight_consumed = api
        .storage()
        .at_latest()
        .await?
        .fetch(&weight_query)
        .await?
        .ok_or("Failed to query consumption")?;

    let weight_limit_query = polkadot::constants().system().block_weights();
    let weight_limit = api.constants().at(&weight_limit_query)?;

    let proof_limit = weight_limit.max_block.proof_size;
    let ref_time_limit = weight_limit.max_block.ref_time;

    let normal_ref_time = weight_consumed.normal.ref_time;
    let operational_ref_time = weight_consumed.operational.ref_time;
    let mandatory_ref_time = weight_consumed.mandatory.ref_time;

    let normal_proof_size = weight_consumed.normal.proof_size;
    let operational_proof_size = weight_consumed.operational.proof_size;
    let mandatory_proof_size = weight_consumed.mandatory.proof_size;

    // Calculate the total proof size
    let total_proof_size = normal_proof_size + operational_proof_size + mandatory_proof_size;

    let consumption = WeightConsumption {
        block_number,
        timestamp,
        ref_time: (
            round_to(normal_ref_time as f32 / ref_time_limit as f32, 3),
            round_to(operational_ref_time as f32 / ref_time_limit as f32, 3),
            round_to(mandatory_ref_time as f32 / ref_time_limit as f32, 3),
        )
        .into(),
        proof_size: (
            round_to(normal_proof_size as f32 / proof_limit as f32, 3),
            round_to(operational_proof_size as f32 / proof_limit as f32, 3),
            round_to(mandatory_proof_size as f32 / proof_limit as f32, 3),
        )
        .into(),
        total_proof_size: round_to(total_proof_size as f32 / proof_limit as f32, 3),  // <-- Correct field added
    };

    Ok(consumption)
}

// Fetch the timestamp for the given block
async fn timestamp_at(
    api: OnlineClient<PolkadotConfig>,
    block_hash: subxt::utils::H256,
) -> Result<Timestamp, Box<dyn std::error::Error>> {
    let timestamp_query = polkadot::storage().timestamp().now();

    let timestamp = api
        .storage()
        .at(block_hash)
        .fetch(&timestamp_query)
        .await?
        .ok_or("Failed to query timestamp")?;

    Ok(timestamp)
}

