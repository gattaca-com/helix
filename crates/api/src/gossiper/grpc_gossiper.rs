use std::{sync::Arc, time::Duration};

use helix_common::{metrics::GossipMetrics, task};
use parking_lot::RwLock;
use prost::Message;
use tokio::{
    sync::mpsc::{self},
    time::sleep,
};
use tonic::{transport::Channel, Request, Response, Status};
use tracing::error;

use crate::{
    gossiper::{
        error::GossipError,
        types::{
            BroadcastGetPayloadParams, BroadcastHeaderParams, BroadcastPayloadParams,
            GossipedMessage, RequestPayloadParams,
        },
    },
    grpc::{
        self,
        gossip_service_client::GossipServiceClient,
        gossip_service_server::{GossipService, GossipServiceServer},
    },
};

const HEADER_ID: &str = "header";
const PAYLOAD_ID: &str = "payload";
const GET_PAYLOAD_ID: &str = "get_payload";
const REQUEST_PAYLOAD_ID: &str = "request_payload";

#[derive(Clone)]
pub struct GrpcGossiperClient {
    endpoint: String,
    client: Arc<RwLock<Option<GossipServiceClient<Channel>>>>,
}

impl GrpcGossiperClient {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint, client: Arc::new(RwLock::new(None)) }
    }

    pub async fn connect(&self) {
        let endpoint = self.endpoint.clone();
        let client = self.client.clone();
        task::spawn(file!(), line!(), async move {
            let mut attempt = 1;
            let base_delay = Duration::from_secs(1);
            let max_delay = Duration::from_secs(60);

            loop {
                match GossipServiceClient::connect(endpoint.clone()).await {
                    Ok(c) => {
                        let mut client_with_lock = client.write();
                        *client_with_lock = Some(c);
                        break;
                    }
                    Err(err) => {
                        error!(%err, "failed to connect to {}", endpoint);
                        let delay = std::cmp::min(base_delay * 2_u32.pow(attempt - 1), max_delay);
                        sleep(delay).await;

                        attempt += 1;
                    }
                }
            }
        });
    }

    fn client(&self) -> Option<GossipServiceClient<Channel>> {
        let client_guard = self.client.read();
        client_guard.clone()
    }

    pub async fn broadcast_header(
        &self,
        request: grpc::BroadcastHeaderParams,
    ) -> Result<(), GossipError> {
        let _timer = GossipMetrics::out_timer(HEADER_ID);
        let size = request.encoded_len();
        GossipMetrics::out_size(HEADER_ID, size);

        let request = Request::new(request);

        if let Some(mut client) = self.client() {
            let result =
                tokio::time::timeout(Duration::from_secs(5), client.broadcast_header(request))
                    .await;
            match result {
                Ok(Ok(_)) => {
                    GossipMetrics::out_count(HEADER_ID, true);
                    Ok(())
                }
                Ok(Err(err)) => {
                    error!(%err, "Client call failed.");
                    GossipMetrics::out_count(HEADER_ID, false);
                    Err(GossipError::BroadcastError(err))
                }
                Err(_) => {
                    error!("Client call timed out.");
                    GossipMetrics::out_count(HEADER_ID, false);
                    Err(GossipError::TimeoutError)
                }
            }
        } else {
            GossipMetrics::out_count(HEADER_ID, false);
            Err(GossipError::ClientNotConnected)
        }
    }

    pub async fn broadcast_payload(
        &self,
        request: grpc::BroadcastPayloadParams,
    ) -> Result<(), GossipError> {
        let _timer = GossipMetrics::out_timer(PAYLOAD_ID);
        let size = request.encoded_len();
        GossipMetrics::out_size(PAYLOAD_ID, size);

        let request = Request::new(request);

        if let Some(mut client) = self.client() {
            let result =
                tokio::time::timeout(Duration::from_secs(5), client.broadcast_payload(request))
                    .await;
            match result {
                Ok(Ok(_)) => {
                    GossipMetrics::out_count(PAYLOAD_ID, true);
                    Ok(())
                }
                Ok(Err(err)) => {
                    error!(%err, "Client call failed.");
                    GossipMetrics::out_count(PAYLOAD_ID, false);
                    Err(GossipError::BroadcastError(err))
                }
                Err(_) => {
                    error!("Client call timed out.");
                    GossipMetrics::out_count(PAYLOAD_ID, false);
                    Err(GossipError::TimeoutError)
                }
            }
        } else {
            GossipMetrics::out_count(PAYLOAD_ID, false);
            Err(GossipError::ClientNotConnected)
        }
    }

    pub async fn broadcast_get_payload(
        &self,
        request: grpc::BroadcastGetPayloadParams,
    ) -> Result<(), GossipError> {
        let _timer = GossipMetrics::out_timer(GET_PAYLOAD_ID);
        let size = request.encoded_len();
        GossipMetrics::out_size(GET_PAYLOAD_ID, size);

        let request = Request::new(request);

        if let Some(mut client) = self.client() {
            let result =
                tokio::time::timeout(Duration::from_secs(5), client.broadcast_get_payload(request))
                    .await;
            match result {
                Ok(Ok(_)) => {
                    GossipMetrics::out_count(GET_PAYLOAD_ID, true);
                    Ok(())
                }
                Ok(Err(err)) => {
                    error!(%err, "Client call failed.");
                    GossipMetrics::out_count(GET_PAYLOAD_ID, false);
                    Err(GossipError::BroadcastError(err))
                }
                Err(_) => {
                    error!("Client call timed out.");
                    GossipMetrics::out_count(GET_PAYLOAD_ID, false);
                    Err(GossipError::TimeoutError)
                }
            }
        } else {
            GossipMetrics::out_count(GET_PAYLOAD_ID, false);
            Err(GossipError::ClientNotConnected)
        }
    }

    pub async fn request_payload(
        &self,
        request: grpc::RequestPayloadParams,
    ) -> Result<(), GossipError> {
        let _timer = GossipMetrics::out_timer(REQUEST_PAYLOAD_ID);
        let size = request.encoded_len();
        GossipMetrics::out_size(REQUEST_PAYLOAD_ID, size);

        let request = Request::new(request);

        if let Some(mut client) = self.client() {
            let result =
                tokio::time::timeout(Duration::from_secs(5), client.request_payload(request)).await;
            match result {
                Ok(Ok(_)) => {
                    GossipMetrics::out_count(REQUEST_PAYLOAD_ID, true);
                    Ok(())
                }
                Ok(Err(err)) => {
                    error!(%err, "Client call failed.");
                    GossipMetrics::out_count(REQUEST_PAYLOAD_ID, false);
                    Err(GossipError::BroadcastError(err))
                }
                Err(_) => {
                    error!("Client call timed out.");
                    GossipMetrics::out_count(REQUEST_PAYLOAD_ID, false);
                    Err(GossipError::TimeoutError)
                }
            }
        } else {
            GossipMetrics::out_count(REQUEST_PAYLOAD_ID, false);
            Err(GossipError::ClientNotConnected)
        }
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
    pub async fn start_server(&self, gossip_sender: mpsc::Sender<GossipedMessage>) {
        let service = GrpcGossiperService { gossip_sender };

        let addr = "0.0.0.0:50051".parse().unwrap();
        task::spawn(file!(), line!(), async move {
            tonic::transport::Server::builder()
                .add_service(GossipServiceServer::new(service))
                .serve(addr)
                .await
                .expect("failed to start gossiper service");
        });
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        Self { clients: vec![] }
    }
}

impl GrpcGossiperClientManager {
    /// Broadcast a header. The header will be saved if it is the best header for the receiving
    /// relay. Only validated Headers are gossiped.
    pub async fn broadcast_header(&self, request: BroadcastHeaderParams) {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            task::spawn(file!(), line!(), async move {
                if let Err(err) = client.broadcast_header(request).await {
                    error!(%err, "failed to broadcast header");
                }
            });
        }
    }

    /// Broadcast a payload. This payload will always be saved to the receiving relay's Autcioneer.
    /// This is because, the local relay has saved the payload's header and may have served it for
    /// get_header. Only validated Payloads are gossiped.
    pub async fn broadcast_payload(&self, request: BroadcastPayloadParams) {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            task::spawn(file!(), line!(), async move {
                if let Err(err) = client.broadcast_payload(request).await {
                    error!(%err, "failed to broadcast payload");
                }
            });
        }
    }

    /// Broadcast a request for a payload. If the receiving relay has the payload, it will be able
    /// to broadcast the block to the network. This is fallback mechanism for when the relay that
    /// get_header was called on does not have the payload yet.
    pub async fn broadcast_get_payload(&self, request: BroadcastGetPayloadParams) {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            task::spawn(file!(), line!(), async move {
                if let Err(err) = client.broadcast_get_payload(request).await {
                    error!( %err, "failed to broadcast get payload");
                }
            });
        }
    }

    pub async fn request_payload(&self, request: RequestPayloadParams) {
        let request = request.to_proto();

        for client in self.clients.iter() {
            let client = client.clone();
            let request = request.clone();
            task::spawn(file!(), line!(), async move {
                if let Err(err) = client.request_payload(request).await {
                    error!(%err, "failed to request payload");
                }
            });
        }
    }
}

/// `GrpcGossiperService` listens to incoming requests from the other geo-distributed instances
/// and processes them.
pub struct GrpcGossiperService {
    gossip_sender: mpsc::Sender<GossipedMessage>,
}

#[tonic::async_trait]
impl GossipService for GrpcGossiperService {
    async fn broadcast_header(
        &self,
        request: Request<grpc::BroadcastHeaderParams>,
    ) -> Result<Response<()>, Status> {
        GossipMetrics::in_count(HEADER_ID);
        let inner = request.into_inner();
        let size = inner.encoded_len();
        GossipMetrics::in_size(HEADER_ID, size);

        let request = match BroadcastHeaderParams::from_proto(inner) {
            Ok(request) => request,
            Err(err) => {
                error!(%err, "failed to decode header");
                return Ok(Response::new(()));
            }
        };
        if let Err(err) = self.gossip_sender.send(GossipedMessage::Header(Box::new(request))).await
        {
            error!(%err, "failed to send header to builder");
        }
        Ok(Response::new(()))
    }

    async fn broadcast_payload(
        &self,
        request: Request<grpc::BroadcastPayloadParams>,
    ) -> Result<Response<()>, Status> {
        GossipMetrics::in_count(PAYLOAD_ID);
        let inner = request.into_inner();
        let size = inner.encoded_len();
        GossipMetrics::in_size(PAYLOAD_ID, size);

        let request = match BroadcastPayloadParams::from_proto(inner) {
            Ok(request) => request,
            Err(err) => {
                error!(%err, "failed to decode payload");
                return Ok(Response::new(()));
            }
        };
        if let Err(err) = self.gossip_sender.send(GossipedMessage::Payload(Box::new(request))).await
        {
            error!(%err, "failed to send payload to builder");
        }

        Ok(Response::new(()))
    }

    async fn broadcast_get_payload(
        &self,
        request: Request<grpc::BroadcastGetPayloadParams>,
    ) -> Result<Response<()>, Status> {
        GossipMetrics::in_count(GET_PAYLOAD_ID);
        let inner = request.into_inner();
        let size = inner.encoded_len();
        GossipMetrics::in_size(GET_PAYLOAD_ID, size);

        let request = match BroadcastGetPayloadParams::from_proto(inner) {
            Ok(request) => request,
            Err(err) => {
                error!(%err, "failed to decode get payload");
                return Ok(Response::new(()));
            }
        };
        if let Err(err) =
            self.gossip_sender.send(GossipedMessage::GetPayload(Box::new(request))).await
        {
            error!(%err, "failed to send get payload to builder");
        }
        Ok(Response::new(()))
    }

    async fn request_payload(
        &self,
        request: Request<grpc::RequestPayloadParams>,
    ) -> Result<Response<()>, Status> {
        GossipMetrics::in_count(REQUEST_PAYLOAD_ID);
        let inner = request.into_inner();
        let size = inner.encoded_len();
        GossipMetrics::in_size(REQUEST_PAYLOAD_ID, size);

        let request = match RequestPayloadParams::from_proto(inner) {
            Ok(request) => request,
            Err(err) => {
                error!(%err, "failed to decode request payload");
                return Ok(Response::new(()));
            }
        };
        if let Err(err) =
            self.gossip_sender.send(GossipedMessage::RequestPayload(Box::new(request))).await
        {
            error!(%err, "failed to send payload to builder");
        }
        Ok(Response::new(()))
    }
}
