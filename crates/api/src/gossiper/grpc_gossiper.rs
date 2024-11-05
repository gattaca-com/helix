use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::{sync::mpsc::Sender, time::sleep};
use tonic::{transport::Channel, Request, Response, Status};
use tracing::error;

use crate::{
    gossiper::{
        error::GossipError,
        traits::GossipClientTrait,
        types::{
            BroadcastGetPayloadParams, BroadcastHeaderParams, BroadcastPayloadParams,
            GossipedMessage,
        },
    },
    grpc::{
        self,
        gossip_service_client::GossipServiceClient,
        gossip_service_server::{GossipService, GossipServiceServer},
    },
};

use super::types::broadcast_cancellation::BroadcastCancellationParams;

#[derive(Clone)]
pub struct GrpcGossiperClient {
    endpoint: String,
    client: Arc<tokio::sync::RwLock<Option<GossipServiceClient<Channel>>>>,
}

impl GrpcGossiperClient {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint, client: Arc::new(tokio::sync::RwLock::new(None)) }
    }

    pub async fn connect(&self) {
        let endpoint = self.endpoint.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let mut attempt = 1;
            let base_delay = Duration::from_secs(1);
            let max_delay = Duration::from_secs(60);

            loop {
                match GossipServiceClient::connect(endpoint.clone()).await {
                    Ok(c) => {
                        let mut client_with_lock = client.write().await;
                        *client_with_lock = Some(c);
                        break
                    }
                    Err(err) => {
                        error!(err = %err, "failed to connect to {}", endpoint);
                        let delay = std::cmp::min(base_delay * 2_u32.pow(attempt - 1), max_delay);
                        sleep(delay).await;

                        attempt += 1;
                    }
                }
            }
        });
    }

    pub async fn broadcast_header(
        &self,
        request: grpc::BroadcastHeaderParams,
    ) -> Result<(), GossipError> {
        let request = Request::new(request);
        let client = {
            let client_guard = self.client.read().await;
            client_guard.clone()
        };

        if let Some(mut client) = client {
            if let Err(err) = client.broadcast_header(request).await {
                return Err(GossipError::BroadcastError(err))
            }
        } else {
            return Err(GossipError::ClientNotConnected)
        }
        Ok(())
    }

    pub async fn broadcast_payload(
        &self,
        request: grpc::BroadcastPayloadParams,
    ) -> Result<(), GossipError> {
        let request = Request::new(request);
        let client = {
            let client_guard = self.client.read().await;
            client_guard.clone()
        };

        if let Some(mut client) = client {
            if let Err(err) = client.broadcast_payload(request).await {
                return Err(GossipError::BroadcastError(err))
            }
        } else {
            return Err(GossipError::ClientNotConnected)
        }
        Ok(())
    }

    pub async fn broadcast_get_payload(
        &self,
        request: grpc::BroadcastGetPayloadParams,
    ) -> Result<(), GossipError> {
        let request = Request::new(request);
        let client = {
            let client_guard = self.client.read().await;
            client_guard.clone()
        };

        if let Some(mut client) = client {
            if let Err(err) = client.broadcast_get_payload(request).await {
                return Err(GossipError::BroadcastError(err))
            }
        } else {
            return Err(GossipError::ClientNotConnected)
        }
        Ok(())
    }

    pub async fn broadcast_cancellation(
        &self,
        request: grpc::BroadcastCancellationParams,
    ) -> Result<(), GossipError> {
        let request = Request::new(request);
        let client = {
            let client_guard = self.client.read().await;
            client_guard.clone()
        };

        if let Some(mut client) = client {
            if let Err(err) = client.broadcast_cancellation(request).await {
                return Err(GossipError::BroadcastError(err))
            }
        } else {
            return Err(GossipError::ClientNotConnected)
        }
        Ok(())
    }
}

/// `GrpcGossiperClientManager` manages multiple gRPC connections used for gossiping new bids
/// across multiple geo-distributed relays.
#[derive(Clone)]
pub struct GrpcGossiperClientManager {
    clients: Vec<GrpcGossiperClient>,
}

impl GrpcGossiperClientManager {
    pub async fn new(endpoints: Vec<String>) -> Result<Self, tonic::transport::Error> {
        let mut clients = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let client = GrpcGossiperClient::new(endpoint);
            client.connect().await;
            clients.push(client);
        }
        Ok(Self { clients })
    }

    /// Starts the gRPC server to listen for gossip requests on the 50051 port.
    /// Will panic if the server can't be started.
    pub async fn start_server(
        &self,
        builder_api_sender: Sender<GossipedMessage>,
        proposer_api_sender: Sender<GossipedMessage>,
    ) {
        let service = GrpcGossiperService { builder_api_sender, proposer_api_sender };

        let addr = "0.0.0.0:50051".parse().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(GossipServiceServer::new(service))
                .serve(addr)
                .await
                .expect("failed to start gossiper service");
        });
    }
}

#[async_trait]
impl GossipClientTrait for GrpcGossiperClientManager {
    async fn broadcast_header(&self, request: BroadcastHeaderParams) -> Result<(), GossipError> {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            tokio::spawn(async move {
                if let Err(err) = client.broadcast_header(request).await {
                    error!(err = %err, "failed to broadcast header");
                }
            });
        }
        Ok(())
    }

    async fn broadcast_payload(&self, request: BroadcastPayloadParams) -> Result<(), GossipError> {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            tokio::spawn(async move {
                if let Err(err) = client.broadcast_payload(request).await {
                    error!(err = %err, "failed to broadcast payload");
                }
            });
        }
        Ok(())
    }

    async fn broadcast_get_payload(
        &self,
        request: BroadcastGetPayloadParams,
    ) -> Result<(), GossipError> {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            tokio::spawn(async move {
                if let Err(err) = client.broadcast_get_payload(request).await {
                    error!(err = %err, "failed to broadcast get payload");
                }
            });
        }
        Ok(())
    }

    async fn broadcast_cancellation(
        &self,
        request: BroadcastCancellationParams,
    ) -> Result<(), GossipError> {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            tokio::spawn(async move {
                if let Err(err) = client.broadcast_cancellation(request).await {
                    error!(err = %err, "failed to broadcast header");
                }
            });
        }
        Ok(())
    }
}

/// `GrpcGossiperService` listens to incoming requests from the other geo-distributed instances
/// and processes them.
pub struct GrpcGossiperService {
    builder_api_sender: Sender<GossipedMessage>,
    proposer_api_sender: Sender<GossipedMessage>,
}

#[tonic::async_trait]
impl GossipService for GrpcGossiperService {
    async fn broadcast_header(
        &self,
        request: Request<grpc::BroadcastHeaderParams>,
    ) -> Result<Response<()>, Status> {
        let request = BroadcastHeaderParams::from_proto(request.into_inner());
        if let Err(err) =
            self.builder_api_sender.send(GossipedMessage::Header(Box::new(request))).await
        {
            error!(err = %err, "failed to send header to builder");
        }
        Ok(Response::new(()))
    }

    async fn broadcast_payload(
        &self,
        request: Request<grpc::BroadcastPayloadParams>,
    ) -> Result<Response<()>, Status> {
        let request = BroadcastPayloadParams::from_proto(request.into_inner());
        if let Err(err) =
            self.builder_api_sender.send(GossipedMessage::Payload(Box::new(request))).await
        {
            error!(err = %err, "failed to send payload to builder");
        }
        Ok(Response::new(()))
    }

    async fn broadcast_get_payload(
        &self,
        request: Request<grpc::BroadcastGetPayloadParams>,
    ) -> Result<Response<()>, Status> {
        let request = BroadcastGetPayloadParams::from_proto(request.into_inner());
        if let Err(err) =
            self.proposer_api_sender.send(GossipedMessage::GetPayload(Box::new(request))).await
        {
            error!(err = %err, "failed to send get payload to builder");
        }
        Ok(Response::new(()))
    }

    async fn broadcast_cancellation(
        &self,
        request: Request<grpc::BroadcastCancellationParams>,
    ) -> Result<Response<()>, Status> {
        let request = BroadcastCancellationParams::from_proto(request.into_inner());
        if let Err(err) =
            self.builder_api_sender.send(GossipedMessage::Cancellation(Box::new(request))).await
        {
            error!(err = %err, "failed to send cancellation to builder");
        }
        Ok(Response::new(()))
    }
}
