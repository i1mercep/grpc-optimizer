//! Continuous LP solving with bounded native workers and four gRPC interaction modes.
//!
//! Use [`engine::Engine`] for admission control and async execution; [`solver::solve`]
//! is the synchronous entry point for callers that manage their own worker capacity.

pub mod config;
pub mod engine;
pub mod grpc;
pub mod model;
pub mod samples;
pub mod solver;

/// Generated wire types, client/server bindings, and the reflection descriptor.
pub mod pb {
    tonic::include_proto!("optimization.v1");
    /// Encoded protobuf descriptor set used by the server's reflection service.
    pub const DESCRIPTOR: &[u8] = tonic::include_file_descriptor_set!("optimizer");
}
