use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use shared::registry::registered_paras;
use std::sync::Arc;
use subxt::{blocks::Block, OnlineClient, PolkadotConfig};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message};
use types::{Parachain, Timestamp, WeightConsumption, RelayChain};
use std::collections::HashMap;
use std::env;
use dotenv::dotenv;

const LOG_TARGET: &str = "tracker";

// Data structures for consumption updates sent over websocket
#[derive(Serialize, Clone)]
struct ConsumptionUpdate {
    para_id: u32,
    relay: RelayChain,
    ref_time: RefTime,
    proof_size: ProofSize,
    total_proof_size: f32,
}

#[derive(Serialize, Clone)]
struct RefTime {
    normal: f32,
    operational: f32,
    mandatory: f32,
}

#[derive(Serialize, Clone)]
struct ProofSize {
    normal: f32,
    operational: f32,
    mandatory: f32,
}

#[subxt::subxt(runtime_metadata_path = "../../artifacts/metadata.scale")]
mod polkadot {}

// Type alias for tracking connected websocket clients
type ClientList =
    Arc<RwLock<Vec<Arc<RwLock<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>>>>>;

// Type alias for the cache of the last messages
type Cache = Arc<RwLock<HashMap<u32, ConsumptionUpdate>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder().filter_level(log::LevelFilter::Debug).init();

    let clients: ClientList = Arc::new(RwLock::new(Vec::new()));
    let cache: Cache = Arc::new(RwLock::new(HashMap::new())); // Initialize the cache

    // Start websocket server for client connections
    let clients_clone = Arc::clone(&clients);
    let cache_clone = Arc::clone(&cache); // Pass the cache to the WebSocket server
    tokio::spawn(async move {
        if let Err(e) = start_websocket_server(clients_clone, cache_clone).await {
            log::error!("WebSocket server encountered an error: {:?}", e);
        }
    });

    // Start tracking parachain data and send updates to websocket clients
    if let Err(e) = start_tracking(0, clients.clone(), cache.clone()).await {
        log::error!("Tracking system encountered an error: {:?}", e);
    }

    Ok(())
}

// Start websocket server to accept client connections
async fn handle_new_connection(
    stream: tokio::net::TcpStream,
    clients: ClientList,
    cache: Cache,
) -> Result<(), Box<dyn std::error::Error>> {
    // Perform WebSocket handshake
    let ws_stream = match accept_async(stream).await {
        Ok(ws_stream) => ws_stream,
        Err(e) => {
            log::error!("Error during WebSocket handshake: {:?}", e);
            return Err(Box::new(e));
        }
    };

    log::debug!("New client connected from {:?}", ws_stream.get_ref().peer_addr());

    let ws_stream = Arc::new(RwLock::new(ws_stream));

    // Add the WebSocket stream to the list of clients
    clients.write().await.push(ws_stream.clone());

    // Send the cached messages to the newly connected client
    let cache_read_guard = cache.read().await;
    for (_, message) in cache_read_guard.iter() {
        let message_json = serde_json::to_string(message)?;
        ws_stream.write().await.send(Message::Text(message_json)).await?;
    }

    Ok(())
}

async fn start_websocket_server(
    clients: ClientList,
    cache: Cache,
) -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok(); // Load environment variables from `.env` file (optional)

    // Read the IP address and port from environment variables, or default to "127.0.0.1:9001"
    let ip = env::var("WEBSOCKET_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = env::var("WEBSOCKET_PORT").unwrap_or_else(|_| "9001".to_string());
    
    // Combine IP and port into the full address
    let addr = format!("{}:{}", ip, port);
    
    let listener = TcpListener::bind(&addr).await?;
    let stream = TcpListenerStream::new(listener);
    log::debug!("WebSocket server started at {}", addr);

    stream
        .for_each(|stream| {
            let clients = Arc::clone(&clients);
            let cache = Arc::clone(&cache); // Clone the cache reference for each new connection
            async move {
                if let Ok(stream) = stream {
                    if let Err(e) = handle_new_connection(stream, clients, cache).await {
                        log::error!("Failed to handle new WebSocket connection: {:?}", e);
                    }
                } else {
                    log::error!("Failed to accept WebSocket connection.");
                }
            }
        })
        .await;

    Ok(())
}

// Start tracking parachain consumption and send updates to websocket clients
async fn start_tracking(
    rpc_index: usize,
    clients: ClientList,
    cache: Cache,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut task_set = tokio::task::JoinSet::new();

    for para in registered_paras() {
        let clients_clone = Arc::clone(&clients);
        let cache_clone = Arc::clone(&cache);
        task_set.spawn(async move {
            track_weight_consumption(para, rpc_index, clients_clone, cache_clone).await;
        });
    }

    // Wait for all tasks to finish
    while let Some(res) = task_set.join_next().await {
        if let Err(e) = res {
            log::error!("A tracking task encountered an error: {:?}", e);
        }
    }

    Ok(())
}

// Track weight consumption for each parachain and broadcast updates to websocket clients
async fn track_weight_consumption(
    para: Parachain,
    rpc_index: usize,
    clients: ClientList,
    cache: Cache,
) {
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
        if let Err(err) = track_blocks(api, para.clone(), rpc_index, clients, cache).await {
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
    cache: Cache,
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
        note_new_block(api.clone(), para.clone(), rpc_index, block, clients.clone(), cache.clone()).await?;
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
    cache: Cache,
) -> Result<(), Box<dyn std::error::Error>> {
    let block_number = block.header().number;
    let timestamp = timestamp_at(api.clone(), block.hash()).await?;
    let consumption = weight_consumption(api, block_number, timestamp).await?;

    let consumption_update = ConsumptionUpdate {
        para_id: para.para_id,
        relay: para.relay_chain,
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

    // Store the latest message in the cache
    {
        let mut cache_write_guard = cache.write().await;
        cache_write_guard.insert(para.para_id, consumption_update.clone());
    }

    // Clone the list of clients while holding the read lock, then release it.
    let client_list_snapshot;
    {
        let clients_read_guard = clients.read().await;
        client_list_snapshot = clients_read_guard.clone(); // Cloning the Arc<RwLock> instances
    }

    let mut disconnected_clients = Vec::new();

    // Now broadcast to the cloned list
    for (i, client) in client_list_snapshot.iter().enumerate() {
        let mut ws_stream = client.write().await;
        if let Err(e) = ws_stream.send(Message::Text(consumption_json.clone())).await {
            log::warn!("Client disconnected: {:?}", e);
            disconnected_clients.push(i);
        }
    }

    // Remove disconnected clients after the broadcast
    remove_disconnected_clients(Arc::clone(&clients), disconnected_clients).await;

    Ok(())
}

async fn remove_disconnected_clients(
    clients: ClientList,
    disconnected_clients: Vec<usize>,
) {
    if disconnected_clients.is_empty() {
        return;
    }

    log::debug!(
        "Attempting to acquire write lock to remove {} disconnected clients at {:?}",
        disconnected_clients.len(),
        std::time::Instant::now()
    );

    match timeout(Duration::from_secs(10), clients.write()).await {
        Ok(mut clients_write_guard) => {
            log::debug!("Write lock acquired at {:?}", std::time::Instant::now());
            for &i in disconnected_clients.iter().rev() {
                clients_write_guard.remove(i);
            }
            log::debug!(
                "Removed {} clients, releasing lock at {:?}",
                disconnected_clients.len(),
                std::time::Instant::now()
            );
        }
        Err(_) => {
            log::error!("Timeout while attempting to remove disconnected clients at {:?}", std::time::Instant::now());
        }
    }
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
            normal_ref_time as f32 / ref_time_limit as f32,
            operational_ref_time as f32 / ref_time_limit as f32,
            mandatory_ref_time as f32 / ref_time_limit as f32,
        )
            .into(),
        proof_size: (
            normal_proof_size as f32 / proof_limit as f32,
            operational_proof_size as f32 / proof_limit as f32,
            mandatory_proof_size as f32 / proof_limit as f32,
        )
            .into(),
        total_proof_size: total_proof_size as f32 / proof_limit as f32,
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

