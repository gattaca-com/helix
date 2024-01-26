use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio::time::sleep;
use tonic::{Response, Status, transport::Channel, Request};
use tracing::error;

use crate::{
    gossiper::{
        error::GossipError,
        traits::GossipClientTrait,
        types::{BroadcastHeaderParams, BroadcastPayloadParams, GossipedMessage},
    },
    grpc::{
        gossip_service_client::GossipServiceClient,
        gossip_service_server::{GossipServiceServer, GossipService},
        self,
    },
};


#[derive(Clone)]
pub struct GrpcGossiperClient {
    endpoint: String,
    client: Arc<Mutex<Option<GossipServiceClient<Channel>>>>,
}

impl GrpcGossiperClient {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint, client: Arc::new(Mutex::new(None)) }
    }

    pub async fn connect(&mut self) {
        let endpoint = self.endpoint.clone();
        let client = self.client.clone();
        tokio::spawn(async move {

            let mut attempt = 1;
            let base_delay = Duration::from_secs(1);
            let max_delay = Duration::from_secs(60);

            loop {
                match GossipServiceClient::connect(endpoint.clone()).await {
                    Ok(c) => {
                        let mut client_with_lock = client.lock().await;
                        *client_with_lock = Some(c);
                        break;
                    },
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
        &mut self,
        request: grpc::BroadcastHeaderParams,
    ) -> Result<(), GossipError> {
        let request = Request::new(request);
        let mut client_guard = self.client.lock().await;

        if let Some(client) = client_guard.as_mut() {
            if let Err(err) = client.broadcast_header(request).await {
                return match err.code() {
                    tonic::Code::Unavailable => {
                        error!(err = %err, "failed to broadcast block");
                        drop(client_guard);
                        // Reconnect
                        self.connect().await;
                        Err(GossipError::BroadcastError(err))
                    },
                    _ => Err(GossipError::BroadcastError(err)),
                }
            }
        } else {
            return Err(GossipError::ClientNotConnected);
        }
        Ok(())
    }

    pub async fn broadcast_payload(
        &mut self,
        request: grpc::BroadcastPayloadParams,
    ) -> Result<(), GossipError> {
        let request = Request::new(request);
        let mut client_guard = self.client.lock().await;

        if let Some(client) = client_guard.as_mut() {
            if let Err(err) = client.broadcast_payload(request).await {
                return match err.code() {
                    tonic::Code::Unavailable => {
                        error!(err = %err, "failed to broadcast block");
                        drop(client_guard);
                        // Reconnect
                        self.connect().await;
                        Err(GossipError::BroadcastError(err))
                    },
                    _ => Err(GossipError::BroadcastError(err)),
                }
            }
        } else {
            return Err(GossipError::ClientNotConnected);
        }
        Ok(())
    }

}


/// `GrpcGossiperClient` manages multiple gRPC connections used for gossiping new bids
/// across multiple geo-distributed relays.
#[derive(Clone)]
pub struct GrpcGossiperClientManager {
    clients: Vec<GrpcGossiperClient>,
}

impl GrpcGossiperClientManager {
    pub async fn new(endpoints: Vec<String>) -> Result<Self, tonic::transport::Error> {
        let mut clients = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let mut client = GrpcGossiperClient::new(endpoint);
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
    ) {
        let service = GrpcGossiperService { builder_api_sender };

        let addr = "0.0.0.0:50051".parse().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(GossipServiceServer::new(service))
                .serve(addr)
                .await.expect("failed to start gossiper service");
        });
    }
}

#[async_trait]
impl GossipClientTrait for GrpcGossiperClientManager {
    async fn broadcast_header(
        &self, 
        request: BroadcastHeaderParams,
    ) -> Result<(), GossipError> {
        let request = request.to_proto();
    
        for client in self.clients.iter() {
            let mut client = client.clone();
            let request = request.clone();
            tokio::spawn(async move {
                if let Err(err) = client.broadcast_header(request).await {
                    error!(err = %err, "failed to broadcast header");
                }
            });
        }
        Ok(())
    }

    async fn broadcast_payload(
        &self, 
        request: BroadcastPayloadParams,
    ) -> Result<(), GossipError> {
        let request = request.to_proto();
    
        for client in self.clients.iter() {
            let mut client = client.clone();
            let request = request.clone();
            tokio::spawn(async move {
                if let Err(err) = client.broadcast_payload(request).await {
                    error!(err = %err, "failed to broadcast payload");
                }
            });
        }
        Ok(())
    }
}

/// `GrpcGossiperService` listens to incoming requests from the other geo-distributed instances
/// and processes them.
pub struct GrpcGossiperService{
    builder_api_sender: Sender<GossipedMessage>,
}

#[tonic::async_trait]
impl GossipService for GrpcGossiperService {
    async fn broadcast_header(
        &self,
        request: Request<grpc::BroadcastHeaderParams>,
    ) -> Result<Response<()>, Status> {
        let request = BroadcastHeaderParams::from_proto(request.into_inner());
        if let Err(err) = self.builder_api_sender.send(GossipedMessage::Header(request)).await {
            error!(err = %err, "failed to send header to builder");
        }
        Ok(Response::new(()))
    }

    async fn broadcast_payload(
        &self,
        request: Request<grpc::BroadcastPayloadParams>,
    ) -> Result<Response<()>, Status> {
        let request = BroadcastPayloadParams::from_proto(request.into_inner());
        if let Err(err) = self.builder_api_sender.send(GossipedMessage::Payload(request)).await {
            error!(err = %err, "failed to send payload to builder");
        }
        Ok(Response::new(()))
    }
}
