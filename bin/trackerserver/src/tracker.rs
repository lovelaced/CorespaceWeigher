use subxt::{blocks::Block, OnlineClient, PolkadotConfig};
use types::{Parachain, WeightConsumption};
use std::error::Error;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::websocket::ClientList;
use futures_util::StreamExt;
use shared::registry::registered_paras;
use shared::consumption::round_to;
use tokio_tungstenite::tungstenite::protocol::Message;
use serde_json;

pub async fn start_tracking(rpc_index: usize, clients: ClientList) -> Result<(), Box<dyn Error>> {
    let tasks: Vec<_> = registered_paras()
        .into_iter()
        .map(|para| {
            let clients_clone = clients.clone();
            tokio::spawn(track_weight_consumption(para, rpc_index, clients_clone))
        })
        .collect();

    for task in tasks {
        task.await??;
    }

    Ok(())
}

async fn track_weight_consumption(
    para: Parachain, 
    rpc_index: usize, 
    clients: ClientList
) -> Result<(), Box<dyn Error>> {
    let Some(rpc) = para.rpcs.get(rpc_index) else {
        log::error!(
            target: "tracker",
            "{}-{} - doesn't have an rpc with index: {}",
            para.relay_chain, para.para_id, rpc_index,
        );
        return Ok(());
    };

    log::info!("{}-{} - Starting to track consumption.", para.relay_chain, para.para_id);
    let api = OnlineClient::<PolkadotConfig>::from_url(rpc).await?;

    track_blocks(api, para.clone(), rpc_index, clients).await
}

async fn track_blocks(
    api: OnlineClient<PolkadotConfig>,
    para: Parachain,
    rpc_index: usize,
    clients: ClientList,
) -> Result<(), Box<dyn Error>> {
    let mut blocks_sub = api
        .blocks()
        .subscribe_finalized()
        .await?;

    while let Some(Ok(block)) = blocks_sub.next().await {
        note_new_block(api.clone(), para.clone(), rpc_index, block, clients.clone()).await?;
    }

    Ok(())
}

async fn note_new_block(
    api: OnlineClient<PolkadotConfig>,
    para: Parachain,
    rpc_index: usize,
    block: Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    clients: ClientList,
) -> Result<(), Box<dyn Error>> {
    let block_number = block.header().number;
    let consumption = weight_consumption(api, block_number).await?;

    let consumption_update = types::ConsumptionUpdate {
        para_id: para.para_id,
        ref_time: types::RefTime {
            normal: consumption.ref_time.normal,
            operational: consumption.ref_time.operational,
            mandatory: consumption.ref_time.mandatory,
        },
        proof_size: types::ProofSize {
            normal: consumption.proof_size.normal,
            operational: consumption.proof_size.operational,
            mandatory: consumption.proof_size.mandatory,
        },
    };

    let consumption_json = serde_json::to_string(&consumption_update)?;

    // Broadcast the consumption update to all connected websocket clients
    let clients = clients.read().await;
    for client in clients.iter() {
        client.send(Message::Text(consumption_json.clone())).await?;
    }

    Ok(())
}

async fn weight_consumption(
    api: OnlineClient<PolkadotConfig>,
    block_number: u32,
) -> Result<WeightConsumption, Box<dyn Error>> {
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

    Ok(WeightConsumption {
        block_number,
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
    })
}

