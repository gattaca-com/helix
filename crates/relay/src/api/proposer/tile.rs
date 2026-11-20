use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use flux::{spine::SpineAdapter, tile::Tile};
use helix_common::{Route, RouterConfig};

use crate::{
    HelixSpine,
    api::server::{ApiServer, Inbound},
};

/// Serves the proposer API without the async runtime.
pub struct ProposerApiTile {
    server: ApiServer,
    requests: Vec<Inbound>,
    terminating: Arc<AtomicBool>,
}

impl ProposerApiTile {
    pub fn new(addrs: &[SocketAddr], config: &RouterConfig, terminating: Arc<AtomicBool>) -> Self {
        let server = ApiServer::new("proposer_api", addrs, config, &[(Route::Status, "GET")]);
        Self { server, requests: Vec::new(), terminating }
    }
}

impl Tile<HelixSpine> for ProposerApiTile {
    fn loop_body(&mut self, adapter: &mut SpineAdapter<HelixSpine>) {
        if self.server.poll(&mut self.requests) {
            adapter.mark_work();
        }
        for request in self.requests.drain(..) {
            match request.route {
                Route::Status => {
                    let terminating = self.terminating.load(Ordering::Relaxed);
                    let status = if terminating { 503 } else { 200 };
                    self.server.respond(request.responder, status, &[], &[]);
                }
                route => unreachable!("{route:?} is not served by this tile"),
            }
        }
    }
}
