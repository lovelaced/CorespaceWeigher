use serde::Serialize;

#[derive(Serialize)]
pub struct ConsumptionUpdate {
    pub para_id: u32,
    pub ref_time: RefTime,
    pub proof_size: ProofSize,
}

#[derive(Serialize)]
pub struct RefTime {
    pub normal: f32,
    pub operational: f32,
    pub mandatory: f32,
}

#[derive(Serialize)]
pub struct ProofSize {
    pub normal: f32,
    pub operational: f32,
    pub mandatory: f32,
}

// Define the structure used to represent weight consumption from the API
pub struct WeightConsumption {
    pub block_number: u32,
    pub ref_time: (f32, f32, f32),
    pub proof_size: (f32, f32, f32),
}

// Parachain struct representing a parachain's relevant info for tracking
pub struct Parachain {
    pub para_id: u32,
    pub relay_chain: String,
    pub rpcs: Vec<String>,
}

