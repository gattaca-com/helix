use std::{sync::Arc, time::Duration};

use axum::{
    Extension,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::StreamExt;
use helix_common::{
    self,
    api::builder_api::TopBidUpdate,
    api_provider::GetHeaderInfo,
    metrics::{TopBidMetrics, TopBidMetricsV2},
};
use hyper::HeaderMap;
use ssz::Encode;
use ssz_derive::{Decode, Encode};
use tokio::time::{self};
use tracing::error;

use super::api::BuilderApi;
use crate::api::{Api, HEADER_API_KEY, HEADER_API_TOKEN, builder::error::BuilderApiError};

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all)]
    pub async fn get_top_bid(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        let Some(api_key) = headers.get(HEADER_API_KEY).or_else(|| headers.get(HEADER_API_TOKEN))
        else {
            return Err(BuilderApiError::InvalidApiKey);
        };

        if !api.local_cache.contains_api_key(api_key) {
            return Err(BuilderApiError::InvalidApiKey);
        }

        let sub = api.top_bid_tx.subscribe();
        Ok(ws.on_upgrade(move |socket| push_top_bids(socket, sub)))
    }

    #[tracing::instrument(skip_all)]
    pub async fn get_top_bid_v2(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        let Some(api_key) = headers.get(HEADER_API_KEY).or_else(|| headers.get(HEADER_API_TOKEN))
        else {
            return Err(BuilderApiError::InvalidApiKey);
        };

        if !api.local_cache.contains_api_key(api_key) {
            return Err(BuilderApiError::InvalidApiKey);
        }

        let topbid_sub = api.top_bid_tx.subscribe();
        let getheader_call_sub = api.getheader_call_tx.subscribe();

        Ok(ws.on_upgrade(move |socket| push_top_bids_v2(socket, topbid_sub, getheader_call_sub)))
    }
}

async fn socket_loop_body(socket: &mut WebSocket, interval: &mut time::Interval) -> bool {
    tokio::select! {
        _ = interval.tick() => {
            if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                error!("Failed to send ping.");
                return true;
            }
        },

        msg = socket.next() => {
            match msg {
                Some(Ok(Message::Ping(data))) => {
                    if socket.send(Message::Pong(data)).await.is_err() {
                        error!("Failed to respond to ping.");
                        return true;
                    }
                },
                Some(Ok(Message::Pong(_))) => {
                },
                Some(Ok(Message::Close(_))) => {
                    return true;
                },
                Some(Ok(Message::Binary(_))) => {
                },
                Some(Ok(Message::Text(_))) => {
                },
                Some(Err(e)) => {
                    error!("Error in WebSocket connection: {}", e);
                    return true;
                },
                None => {
                    error!("WebSocket connection closed by the other side.");
                    return true;
                }
            }
        }
    }
    false
}

/// `push_top_bids` manages a WebSocket connection to continuously send the top auction bids to a
/// client.
///
/// - Periodically fetches the latest auction bids via a stream and sends them to the client in ssz
///   format.
/// - Sends a ping message every 10 seconds to maintain the connection's liveliness.
/// - Terminates the connection on sending failures or if a bid stream error occurs, ensuring clean
///   disconnection.
///
/// This function operates in an asynchronous loop until the WebSocket connection is closed either
/// due to an error or when the auction ends. It returns after the socket has been closed, logging
/// the closure status.
async fn push_top_bids(
    mut socket: WebSocket,
    mut bid_stream: tokio::sync::broadcast::Receiver<TopBidUpdate>,
) {
    let _conn = TopBidMetrics::connection();
    let mut interval = time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            Ok(bid) = bid_stream.recv() => {
                if socket.send(Message::Binary(bid.as_ssz_bytes_fast().into())).await.is_err() {
                    error!("Failed to send bid. Disconnecting.");
                    break;
                }
            },
            should_break = socket_loop_body(&mut socket, &mut interval) => {
                if should_break {
                    break;
                }

            }
        }
    }
    tracing::debug!("Socket connection closed gracefully.");
}

#[derive(Clone, Decode, Encode)]
#[ssz(enum_behaviour = "union")]
pub enum TopBidV2 {
    TopBid(TopBidUpdate),
    AuctionOpen(bool),
}

impl TopBidV2 {
    pub fn as_ssz_bytes_fast(&self) -> Vec<u8> {
        const SIZE_TOPBID: usize = 189;
        const SIZE_AUCTION_OPEN: usize = 2;
        match self {
            s @ TopBidV2::TopBid(_) => {
                let mut v = Vec::with_capacity(SIZE_TOPBID);
                s.ssz_append(&mut v);
                v
            }
            s @ TopBidV2::AuctionOpen(_) => {
                let mut v = Vec::with_capacity(SIZE_AUCTION_OPEN);
                s.ssz_append(&mut v);
                v
            }
        }
    }
}

/// `push_top_bids_v2` manages a WebSocket connection to continuously send the top auction bids, and
/// proposer getheader call information to a client.
///
/// - Sends the latest auction bids and proposer getheader call information to the client in ssz
///   format.
/// - Sends a ping message every 10 seconds to maintain the connection's liveliness.
/// - Terminates the connection on sending failures or if a bid stream error occurs, ensuring clean
///   disconnection.
///
/// This function operates in an asynchronous loop until the WebSocket connection is closed either
/// due to an error or when the auction ends. It returns after the socket has been closed, logging
/// the closure status.
async fn push_top_bids_v2(
    mut socket: WebSocket,
    mut bid_stream: tokio::sync::broadcast::Receiver<TopBidUpdate>,
    mut getheader_call_stream: tokio::sync::broadcast::Receiver<GetHeaderInfo>,
) {
    let _conn = TopBidMetricsV2::connection();
    let mut interval = time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            should_break = socket_loop_body(&mut socket, &mut interval) => {
                if should_break {
                    break;
                }

            }
            Ok(bid) = bid_stream.recv() => {
                if socket.send(Message::Binary(TopBidV2::TopBid(bid).as_ssz_bytes_fast().into())).await.is_err() {
                    error!("Failed to send bid. Disconnecting.");
                    break;
                }
            },
            Ok(getheader) = getheader_call_stream.recv() => {
                if socket.send(Message::Binary(TopBidV2::AuctionOpen(getheader.called).as_ssz_bytes_fast().into())).await.is_err() {
                    error!("Failed to send bid. Disconnecting.");
                    break;
                }
            },
        }
    }

    tracing::debug!("Socket connection closed gracefully.");
}
