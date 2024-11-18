use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use serde::Serialize;
use subxt::{blocks::Block, OnlineClient, PolkadotConfig, utils::H256};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};
use warp::sse::Event;
use warp::{http::Method, Filter};
use types::{Parachain, RelayChain, WeightConsumption, Timestamp};
use shared::registry::registered_paras;

const LOG_TARGET: &str = "tracker";

#[derive(Serialize, Clone)]
struct ConsumptionUpdate {
    para_id: u32,
    relay: RelayChain,
    block_number: u32,
    extrinsics_num: usize,
    authorities_num: usize,
    timestamp: Timestamp,
    block_time_seconds: Option<f64>,
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

type Clients = Arc<RwLock<Vec<mpsc::UnboundedSender<Result<Event, Infallible>>>>>;
type Cache = Arc<RwLock<HashMap<u32, ConsumptionUpdate>>>;
type TimestampCache = Arc<RwLock<HashMap<u32, (u32, Timestamp, Option<f64>)>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder().filter_level(log::LevelFilter::Debug).init();

    let clients: Clients = Arc::new(RwLock::new(Vec::new()));
    let cache: Cache = Arc::new(RwLock::new(HashMap::new()));
    let timestamp_cache: TimestampCache = Arc::new(RwLock::new(HashMap::new()));

    let clients_clone = Arc::clone(&clients);
    let cache_clone = Arc::clone(&cache);
    tokio::spawn(async move {
        if let Err(e) = start_sse_server(clients_clone, cache_clone).await {
            log::error!("SSE server encountered an error: {:?}", e);
        }
    });

    if let Err(e) = start_tracking(0, clients.clone(), cache.clone(), timestamp_cache.clone()).await {
        log::error!("Tracking system encountered an error: {:?}", e);
    }

    Ok(())
}

async fn start_sse_server(clients: Clients, cache: Cache) -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();

    let ip = env::var("SSE_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = env::var("SSE_PORT").unwrap_or_else(|_| "9001".to_string());
    let addr: std::net::SocketAddr = format!("{}:{}", ip, port).parse()?;

    let clients_filter = warp::any().map(move || clients.clone());
    let cache_filter = warp::any().map(move || cache.clone());

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
                let (tx, rx) = mpsc::unbounded_channel();

                {
                    let cache = cache.read().await;
                    for (_, message) in cache.iter() {
                        let data = serde_json::to_string(message).unwrap();
                        let event_id = format!("{}-{}-{}", message.relay, message.para_id, message.block_number);
                        let sse_event = Event::default()
                            .id(event_id)
                            .event("consumptionUpdate")
                            .data(data);
                        let _ = tx.send(Ok(sse_event));
                    }
                }
                clients.write().await.push(tx);
                let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
                Ok::<_, Infallible>(warp::sse::reply(warp::sse::keep_alive().stream(stream)))
            },
        )
        .with(cors);

    warp::serve(routes).run(addr).await;

    Ok(())
}

async fn start_tracking(
    rpc_index: usize,
    clients: Clients,
    cache: Cache,
    timestamp_cache: TimestampCache,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut task_set = JoinSet::new();

    for para in registered_paras() {
        let clients_clone = Arc::clone(&clients);
        let cache_clone = Arc::clone(&cache);
        let timestamp_cache_clone = Arc::clone(&timestamp_cache);
        task_set.spawn(async move {
            track_weight_consumption(para, rpc_index, clients_clone, cache_clone, timestamp_cache_clone).await;
        });
    }

    while let Some(res) = task_set.join_next().await {
        if let Err(e) = res {
            log::error!("A tracking task encountered an error: {:?}", e);
        }
    }

    Ok(())
}

async fn track_weight_consumption(
    para: Parachain,
    rpc_index: usize,
    clients: Clients,
    cache: Cache,
    timestamp_cache: TimestampCache,
) {
    let Some(rpc) = para.rpcs.get(rpc_index) else {
        log::error!(
            target: LOG_TARGET,
            "{}-{} - doesn't have an rpc with index: {}",
            para.relay_chain, para.para_id, rpc_index,
        );
        return;
    };

    let mut retry_delay = Duration::from_secs(1);
    let max_retry_delay = Duration::from_secs(60);

    loop {
        log::info!(
            "{}-{} - Attempting to connect to RPC at {}",
            para.relay_chain,
            para.para_id,
            rpc
        );

        match OnlineClient::<PolkadotConfig>::from_url(rpc).await {
            Ok(api) => {
                // Reset retry delay upon successful connection
                retry_delay = Duration::from_secs(1);

                match track_blocks(
                    api,
                    para.clone(),
                    clients.clone(),
                    cache.clone(),
                    timestamp_cache.clone(),
                )
                .await
                {
                    Ok(_) => {
                        // The function returned normally, which shouldn't happen; log and retry
                        log::warn!(
                            "{}-{} - track_blocks returned without error, but subscription ended unexpectedly.",
                            para.relay_chain,
                            para.para_id
                        );
                    }
                    Err(err) => {
                        log::error!(
                            target: LOG_TARGET,
                            "{}-{} - Error in track_blocks: {:?}",
                            para.relay_chain,
                            para.para_id,
                            err
                        );
                    }
                }
            }
            Err(err) => {
                log::error!(
                    target: LOG_TARGET,
                    "{}-{} - Failed to create online client: {:?}",
                    para.relay_chain,
                    para.para_id,
                    err
                );
            }
        }

        // Wait before retrying to avoid tight loop
        log::info!(
            "{}-{} - Waiting {:?} before retrying connection",
            para.relay_chain,
            para.para_id,
            retry_delay
        );
        tokio::time::sleep(retry_delay).await;

        // Increase the delay for the next retry, up to the maximum
        retry_delay = std::cmp::min(retry_delay * 2, max_retry_delay);
    }
}

async fn track_blocks(
    api: OnlineClient<PolkadotConfig>,
    para: Parachain,
    clients: Clients,
    cache: Cache,
    timestamp_cache: TimestampCache,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut retry_delay = Duration::from_secs(1);
    let max_retry_delay = Duration::from_secs(60);
    let block_timeout = Duration::from_secs(60); // Set your desired timeout duration here

    loop {
        log::info!(
            "{}-{} - Subscribing to best blocks at {:?}",
            para.relay_chain,
            para.para_id,
            Instant::now()
        );

        match api.blocks().subscribe_best().await {
            Ok(mut blocks_sub) => {
                // Reset retry delay upon successful subscription
                retry_delay = Duration::from_secs(1);

                loop {
                    match timeout(block_timeout, blocks_sub.next()).await {
                        Ok(Some(Ok(block))) => {
                            // Reset retry delay upon receiving a block
                            retry_delay = Duration::from_secs(1);

                            if let Err(err) = note_new_block(
                                api.clone(),
                                para.clone(),
                                block,
                                clients.clone(),
                                cache.clone(),
                                timestamp_cache.clone(),
                            )
                            .await
                            {
                                log::error!(
                                    "{}-{} - Error processing new block: {:?}",
                                    para.relay_chain,
                                    para.para_id,
                                    err
                                );
                            }
                        }
                        Ok(Some(Err(err))) => {
                            log::error!(
                                "{}-{} - Error receiving block: {:?}",
                                para.relay_chain,
                                para.para_id,
                                err
                            );
                            // Break to recreate the client
                            return Err(Box::new(err));
                        }
                        Ok(None) => {
                            log::warn!(
                                "{}-{} - Block stream ended unexpectedly",
                                para.relay_chain,
                                para.para_id
                            );
                            // Break to recreate the client
                            return Ok(());
                        }
                        Err(_) => {
                            log::warn!(
                                "{}-{} - No new block received in {:?}, reconnecting",
                                para.relay_chain,
                                para.para_id,
                                block_timeout
                            );
                            // Break to recreate the client
                            return Ok(());
                        }
                    }
                }
            }
            Err(err) => {
                log::error!(
                    "{}-{} - Failed to subscribe to best blocks: {:?}",
                    para.relay_chain,
                    para.para_id,
                    err
                );
                // Break to recreate the client
                return Err(Box::new(err));
            }
        }
    }
}

async fn note_new_block(
    api: OnlineClient<PolkadotConfig>,
    para: Parachain,
    block: Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    clients: Clients,
    cache: Cache,
    timestamp_cache: TimestampCache,
) -> Result<(), Box<dyn std::error::Error>> {
    let block_number = block.header().number;
    let extrinsics_num = block.extrinsics().await?.len();
    let authorities_num = authorities_num(&api, block.hash()).await?;
    let timestamp = timestamp_at(api.clone(), block.hash()).await?;
    let consumption = weight_consumption(api, block_number).await?;
    let relay_chain = para.relay_chain.clone();

    // Retrieve the previous block data if available
    let (last_block_number, last_timestamp, last_valid_time) = {
        let cache = timestamp_cache.read().await;
        cache.get(&para.para_id)
             .map(|(last_block_number, last_timestamp, last_valid_time)| (*last_block_number, *last_timestamp, *last_valid_time))
             .unwrap_or((0, 0, None)) // Defaults if no previous entry exists
    };

    // Calculate block time in seconds if blocks are sequential; otherwise, use last valid time
    let block_time_seconds = if last_block_number + 1 == block_number {
        let difference = (timestamp - last_timestamp) as f64 / 1000.0;
        Some(difference)
    } else {
        last_valid_time
    };

    // Update the timestamp cache with the latest block data
    {
        let mut cache = timestamp_cache.write().await;
        cache.insert(para.para_id, (block_number, timestamp, block_time_seconds));
    }

    let consumption_update = ConsumptionUpdate {
        para_id: para.para_id,
        relay: relay_chain.clone(),
        block_number,
        extrinsics_num,
        authorities_num,
        timestamp,
        block_time_seconds,
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
    let event_id = format!("{}-{}-{}", relay_chain, para.para_id, block_number);

    {
        let mut cache_write_guard = cache.write().await;
        cache_write_guard.insert(para.para_id, consumption_update.clone());
    }

    let client_list_snapshot = {
        let clients_read_guard = clients.read().await;
        clients_read_guard.clone()
    };

    let mut disconnected_clients = Vec::new();
    for (i, client) in client_list_snapshot.iter().enumerate() {
        let sse_event = Event::default()
            .id(event_id.clone())
            .event("consumptionUpdate")
            .data(consumption_json.clone());
        if client.send(Ok(sse_event)).is_err() {
            log::warn!("Client disconnected");
            disconnected_clients.push(i);
        }
    }

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

async fn authorities_num(
    api: &OnlineClient<PolkadotConfig>,
    block_hash: H256,
) -> Result<usize, Box<dyn std::error::Error>> {
    let storage_query = polkadot::storage().session().validators();
    let result = api.storage().at(block_hash).fetch(&storage_query).await;

    match result {
        Ok(Some(validators)) => Ok(validators.len()),
        Ok(None) => Ok(0),
        Err(e) => {
            log::debug!("Failed to fetch session.validators: {:?}. Assuming 0 authorities.", e);
            Ok(0)
        }
    }
}

async fn timestamp_at(
    api: OnlineClient<PolkadotConfig>,
    block_hash: H256,
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

