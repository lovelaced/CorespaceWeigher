use serde::Serialize;
use shared::registry::registered_paras;
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use subxt::{blocks::Block, OnlineClient, PolkadotConfig};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};
use warp::sse::Event;
use warp::{http::Method, Filter};
use types::{Parachain, RelayChain, WeightConsumption};

const LOG_TARGET: &str = "tracker";

// Data structures for consumption updates sent over SSE
#[derive(Serialize, Clone)]
struct ConsumptionUpdate {
    para_id: u32,
    relay: RelayChain,
    block_number: u32,
    extrinsics_num: usize,
    authorities_num: usize, // New field
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

// Type alias for tracking connected SSE clients
type Clients = Arc<RwLock<Vec<mpsc::UnboundedSender<Result<Event, Infallible>>>>>;

// Type alias for the cache of the last messages
type Cache = Arc<RwLock<HashMap<u32, ConsumptionUpdate>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder().filter_level(log::LevelFilter::Debug).init();

    let clients: Clients = Arc::new(RwLock::new(Vec::new()));
    let cache: Cache = Arc::new(RwLock::new(HashMap::new())); // Initialize the cache

    // Start SSE server for client connections
    let clients_clone = Arc::clone(&clients);
    let cache_clone = Arc::clone(&cache); // Pass the cache to the SSE server
    tokio::spawn(async move {
        if let Err(e) = start_sse_server(clients_clone, cache_clone).await {
            log::error!("SSE server encountered an error: {:?}", e);
        }
    });

    // Start tracking parachain data and send updates to SSE clients
    if let Err(e) = start_tracking(0, clients.clone(), cache.clone()).await {
        log::error!("Tracking system encountered an error: {:?}", e);
    }

    Ok(())
}

// Start SSE server to accept client connections
async fn start_sse_server(
    clients: Clients,
    cache: Cache,
) -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok(); // Load environment variables from `.env` file (optional)

    // Read the IP address and port from environment variables, or default to "127.0.0.1:9001"
    let ip = env::var("WEBSOCKET_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = env::var("WEBSOCKET_PORT").unwrap_or_else(|_| "9001".to_string());

    // Combine IP and port into the full address
    let addr_str = format!("{}:{}", ip, port);
    let addr: std::net::SocketAddr = addr_str.parse()?;

    let clients_filter = warp::any().map(move || clients.clone());
    let cache_filter = warp::any().map(move || cache.clone());

    // Configure CORS
    let cors = warp::cors()
        .allow_any_origin()
        .allow_methods(vec![Method::GET])
        .allow_headers(vec![
            "Content-Type",
            "Cache-Control",
            "X-Requested-With",
            "Last-Event-ID",
        ]);

    let routes = warp::path("events")
        .and(warp::get())
        .and(clients_filter)
        .and(cache_filter)
        .and_then(
            |clients: Clients, cache: Cache| async move {
                // Create an unbounded channel to send SSE events
                let (tx, rx) = mpsc::unbounded_channel();

                // Send cached messages to the new client
                {
                    let cache = cache.read().await;
                    for (_, message) in cache.iter() {
                        let data = serde_json::to_string(message).unwrap();
                        // Generate an event ID based on para_id and block_number
                        let event_id = format!("{}-{}", message.para_id, message.block_number);
                        // Create the SSE event with the same format as regular messages
                        let sse_event = Event::default()
                            .id(event_id)
                            .event("consumptionUpdate")
                            .data(data);
                        let _ = tx.send(Ok(sse_event));
                    }
                }
                // Add the sender to the list of clients
                clients.write().await.push(tx);

                // Create a stream from the receiver
                let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);

                // Reply using server-sent events
                Ok::<_, Infallible>(warp::sse::reply(warp::sse::keep_alive().stream(stream)))
            },
        )
        .with(cors);

    warp::serve(routes).run(addr).await;

    Ok(())
}

// Start tracking parachain consumption and send updates to SSE clients
async fn start_tracking(
    rpc_index: usize,
    clients: Clients,
    cache: Cache,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut task_set = JoinSet::new();

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

// Track weight consumption for each parachain and broadcast updates to SSE clients
async fn track_weight_consumption(
    para: Parachain,
    rpc_index: usize,
    clients: Clients,
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

    log::info!(
        "{}-{} - Starting to track consumption.",
        para.relay_chain,
        para.para_id
    );
    let result = OnlineClient::<PolkadotConfig>::from_url(rpc).await;

    if let Ok(api) = result {
        if let Err(err) = track_blocks(api, para.clone(), clients, cache).await {
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
    clients: Clients,
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
        note_new_block(
            api.clone(),
            para.clone(),
            block,
            clients.clone(),
            cache.clone(),
        )
        .await?;
    }

    Ok(())
}

// Collect consumption data for each finalized block and broadcast to SSE clients
async fn note_new_block(
    api: OnlineClient<PolkadotConfig>,
    para: Parachain,
    block: Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    clients: Clients,
    cache: Cache,
) -> Result<(), Box<dyn std::error::Error>> {
    let block_number = block.header().number;
    let extrinsics = block.extrinsics().await?;
    let extrinsics_num = extrinsics.len();

    // Fetch the number of authorities at the block's hash
    let authorities_num = authorities_num(&api, block.hash()).await?;

    let consumption = weight_consumption(api, block_number).await?;

    let consumption_update = ConsumptionUpdate {
        para_id: para.para_id,
        relay: para.relay_chain,
        block_number,
        extrinsics_num,
        authorities_num, // Include the number of authorities
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
        client_list_snapshot = clients_read_guard.clone(); // Cloning the UnboundedSender instances
    }

    let mut disconnected_clients = Vec::new();

    // Now broadcast to the cloned list
    for (i, client) in client_list_snapshot.iter().enumerate() {
        let event_id = format!("{}-{}", para.para_id, block_number);
        // Create the SSE event with custom event type and ID
        let sse_event = Event::default()
            .id(event_id)
            .event("consumptionUpdate")
            .data(consumption_json.clone());
        if client.send(Ok(sse_event)).is_err() {
            log::warn!("Client disconnected");
            disconnected_clients.push(i);
        }
    }

    // Remove disconnected clients after the broadcast
    remove_disconnected_clients(Arc::clone(&clients), disconnected_clients).await;

    Ok(())
}

async fn remove_disconnected_clients(clients: Clients, disconnected_clients: Vec<usize>) {
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
            log::error!(
                "Timeout while attempting to remove disconnected clients at {:?}",
                std::time::Instant::now()
            );
        }
    }
}

// Fetch weight consumption data for the given block
async fn weight_consumption(
    api: OnlineClient<PolkadotConfig>,
    block_number: u32,
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
async fn authorities_num(
    api: &OnlineClient<PolkadotConfig>,
    block_hash: subxt::utils::H256,
) -> Result<usize, Box<dyn std::error::Error>> {
    // Build the storage query for session.validators
    let storage_query = polkadot::storage().session().validators();

    // Attempt to fetch the list of validators at the given block hash
    let result = api
        .storage()
        .at(block_hash)
        .fetch(&storage_query)
        .await;

    match result {
        Ok(Some(validators)) => {
            // Return the number of validators
            Ok(validators.len())
        }
        Ok(None) => {
            // Storage item exists but no validators found
            Ok(0)
        }
        Err(e) => {
            // Log a warning and return 0
            log::debug!(
                "Failed to fetch session.validators: {:?}. Assuming 0 authorities.",
                e
            );
            Ok(0)
        }
    }
}

