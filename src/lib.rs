pub mod config;
pub mod engine;
pub mod grpc;
pub mod model;
pub mod samples;
pub mod solver;

pub mod pb {
    tonic::include_proto!("optimization.v1");
    pub const DESCRIPTOR: &[u8] = tonic::include_file_descriptor_set!("optimizer");
}
