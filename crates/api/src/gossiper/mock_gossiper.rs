use std::convert::TryFrom;

use async_trait::async_trait;
use tonic::{Response, Status, Request};
use crate::{gossiper::{
    error::GossipError,
    traits::GossipClientTrait,
}, grpc::{
    gossip_service_server::GossipService,
}, grpc};
use crate::builder::SubmitBlockParams;


#[derive(Clone)]
pub struct MockGossiper { }

impl MockGossiper {
    pub fn new() -> Result<Self, tonic::transport::Error> {
        Ok(Self {  })
    }

    pub async fn start_server(
        &self,
    ) -> Result<(), tonic::transport::Error> {
        Ok(())
    }
}

#[async_trait]
impl GossipClientTrait for MockGossiper {
    async fn broadcast_block(&self, _req: &SubmitBlockParams) -> Result<(), GossipError> {
        Ok(())
    }
}

pub struct MockGossiperService{ }

#[tonic::async_trait]
impl GossipService for MockGossiperService {
    async fn broadcast_block(
        &self,
        _request: Request<grpc::SubmitBlockParams>,
    ) -> Result<Response<()>, Status> {
        Ok(tonic::Response::new(()))
    }
}
