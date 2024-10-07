use async_trait::async_trait;
use tonic::{Request, Response, Status};

use crate::{
    gossiper::{
        error::GossipError,
        traits::GossipClientTrait,
        types::{BroadcastGetPayloadParams, BroadcastHeaderParams, BroadcastPayloadParams},
    },
    grpc::{self, gossip_service_server::GossipService},
};

#[derive(Clone)]
pub struct MockGossiper {}

impl MockGossiper {
    pub fn new() -> Result<Self, tonic::transport::Error> {
        Ok(Self {})
    }

    pub async fn start_server(&self) -> Result<(), tonic::transport::Error> {
        Ok(())
    }
}

#[async_trait]
impl GossipClientTrait for MockGossiper {
    async fn broadcast_header(&self, _request: BroadcastHeaderParams) -> Result<(), GossipError> {
        Ok(())
    }
    async fn broadcast_payload(&self, _request: BroadcastPayloadParams) -> Result<(), GossipError> {
        Ok(())
    }
    async fn broadcast_get_payload(&self, _request: BroadcastGetPayloadParams) -> Result<(), GossipError> {
        Ok(())
    }
}

pub struct MockGossiperService {}

#[tonic::async_trait]
impl GossipService for MockGossiperService {
    async fn broadcast_header(&self, _request: Request<grpc::BroadcastHeaderParams>) -> Result<Response<()>, Status> {
        Ok(tonic::Response::new(()))
    }

    async fn broadcast_payload(&self, _request: Request<grpc::BroadcastPayloadParams>) -> Result<Response<()>, Status> {
        Ok(tonic::Response::new(()))
    }

    async fn broadcast_get_payload(&self, _request: Request<grpc::BroadcastGetPayloadParams>) -> Result<Response<()>, Status> {
        Ok(tonic::Response::new(()))
    }
}
