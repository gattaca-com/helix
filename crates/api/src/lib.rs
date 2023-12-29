pub mod builder;
pub mod integration_tests;
pub mod proposer;
pub mod relay_data;
pub mod service;
pub mod gossiper;
pub mod router;

#[cfg(test)]
pub mod test_utils;


mod grpc {
    include!(concat!(env!("OUT_DIR"), "/gossip.rs"));
}