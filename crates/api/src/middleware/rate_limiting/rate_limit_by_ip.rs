use std::{collections::HashMap, net::{IpAddr, SocketAddr}, sync::{Arc, Mutex}, time::{Duration, Instant}};
use axum::{
    extract::{connect_info, Request, State}, middleware::Next, response::{IntoResponse, Response}
};

use super::error::RateLimitExceeded;

#[derive(Clone)]
pub struct RateLimitState {
    state_per_route: Arc<HashMap<String, RateLimitStateForRoute>>,
}

impl RateLimitState {
    pub fn new(rate_limits: HashMap<String, RateLimitStateForRoute>) -> Self {
        RateLimitState {
            state_per_route: Arc::new(rate_limits),
        }
    }

    pub fn check_rate_limit(&self, ip: IpAddr, route: &str) -> bool {
        if let Some(state) = self.state_per_route.get(replace_dynamic_routes(route)) {
            state.check_rate_limit(ip)
        } else {
            true
        }
    }
}

// TODO: feels a bit hacky, maybe there's a better way to do this?
fn replace_dynamic_routes(route: &str) -> &str {
    // Currently, the only dynamic route is /eth/v1/builder/header/:slot/:parent_hash/:pubkey
    if route.starts_with("/eth/v1/builder/header") {
        "/eth/v1/builder/header/:slot/:parent_hash/:pubkey"
    } else {
        route
    }
}

/// Represents the rate limiting entry for an IP address.
#[derive(Debug, Clone)]
struct RateLimitEntry {
    request_count: usize,
    last_access: Instant,
}

impl Default for RateLimitEntry {
    fn default() -> Self {
        RateLimitEntry {
            request_count: 0,
            last_access: Instant::now(),
        }
    }
}

/// Represents the state of rate limiting for each IP address.
#[derive(Clone)]
pub struct RateLimitStateForRoute {
    ip_counts: Arc<Mutex<HashMap<IpAddr, RateLimitEntry>>>,
    limit_duration: Duration,
    max_requests: usize,
    max_entries: usize,
}

impl RateLimitStateForRoute {
    /// Create a new rate limiting state.
    pub fn new(limit_duration: Duration, max_requests: usize) -> Self {
        RateLimitStateForRoute {
            ip_counts: Arc::new(Mutex::new(HashMap::new())),
            limit_duration,
            max_requests,
            max_entries: 100_000,
        }
    }

    /// Checks if the IP address is within the rate limit.
    fn check_rate_limit(&self, ip: IpAddr) -> bool {
        // Access the rate limiting state
        let mut ip_counts = self.ip_counts.lock().unwrap();

        // Cleanup the HashMap if it exceeds the maximum number of entries
        if ip_counts.len() > self.max_entries {
            self.prune_entries(&mut ip_counts);
        }

        // Get or insert the IP address entry in the hashmap
        let entry = ip_counts.entry(ip).or_insert_with(|| RateLimitEntry::default());

        // Update the access time and request count for the IP address
        let elapsed = entry.last_access.elapsed();
        if elapsed >= self.limit_duration {
            // Reset the request count if more than the limit duration has passed
            entry.request_count = 1;
            entry.last_access = Instant::now();
            true
        } else if entry.request_count >= self.max_requests {
            // Reject the request if the request count exceeds the maximum requests
            false
        } else {
            // Increment the request count if within the rate limit
            entry.request_count += 1;
            true
        }
    }

    /// Prune the HashMap to reduce the number of entries.
    fn prune_entries(&self, ip_counts: &mut HashMap<IpAddr, RateLimitEntry>) {
        // Sort the entries by access time and limit the number of entries
        let mut entries: Vec<_> = ip_counts.iter().collect();
        entries.sort_by_key(|(_, entry)| entry.last_access);
        entries.truncate(100); // Limit the number of entries to 100

        // Reconstruct the HashMap with pruned entries
        let pruned_ip_counts: HashMap<_, _> = entries.into_iter().map(|(&ip, entry)| (ip, entry.clone())).collect();
        *ip_counts = pruned_ip_counts;
    }
}


pub async fn rate_limit_by_ip(
    State(state): State<RateLimitState>,
    connect_info: connect_info::ConnectInfo::<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {

    let timestamp = Instant::now();
    let ip = connect_info.0.ip();

    let route = request.uri().path();

    // Check if the IP address is within the rate limit
    if !state.check_rate_limit(ip, route) {
        return RateLimitExceeded::new().into_response();
    }

    let elapsed = timestamp.elapsed();
    println!("Request from {} for route {} took {:?}", ip, route, elapsed);

    // Execute the remaining middleware stack.
    next.run(request).await
}