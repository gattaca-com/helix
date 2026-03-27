use std::{convert::TryFrom, task::Poll};

use alloy_primitives::U256;
use bytes::Bytes;
use ethers::abi::{Abi, Token};
use helix_common::{
    BuilderConfig, BuilderInfo, PrimevConfig, ProposerDuty,
    http::client::{HttpClient, PendingResponse},
    local_cache::LocalCache,
};
use helix_types::BlsPublicKeyBytes;
use serde::Deserialize;
use tracing::{debug, error};
use url::Url;

pub const PRIMEV_BUILDER_ID: &str = "PrimevBuilder";

const VALIDATOR_ABI: &str = r#"[{
    "type": "function",
    "name": "areValidatorsOptedIn",
    "inputs": [{"name": "valBLSPubKeys", "type": "bytes[]", "internalType": "bytes[]"}],
    "outputs": [{"components": [
        {"name": "isVanillaOptedIn", "type": "bool", "internalType": "bool"},
        {"name": "isAvsOptedIn", "type": "bool", "internalType": "bool"},
        {"name": "isMiddlewareOptedIn", "type": "bool", "internalType": "bool"}
    ], "name": "", "type": "tuple[]", "internalType": "struct IValidatorOptInRouter.OptInStatus[]"}],
    "stateMutability": "view"
}]"#;

#[derive(Deserialize)]
struct EthJsonRpcResult<T> {
    result: T,
}

#[derive(Deserialize)]
struct EthLog {
    data: String,
}

/// BLSKeyAdded(address indexed provider, bytes blsPublicKey)
fn bls_key_added_topic() -> String {
    let hash = alloy_primitives::keccak256("BLSKeyAdded(address,bytes)");
    format!("0x{}", alloy_primitives::hex::encode(hash.as_slice()))
}

fn process_bls_key_data(data: &[u8]) -> Option<BlsPublicKeyBytes> {
    debug!(raw = alloy_primitives::hex::encode_prefixed(data), "Raw BLS key data");

    if let Ok(key) = BlsPublicKeyBytes::try_from(data) {
        return Some(key);
    }

    if data.len() < 64 {
        return None;
    }

    let data_part = &data[64..];
    if data_part.len() < 48 {
        return None;
    }

    BlsPublicKeyBytes::try_from(&data_part[..48]).ok()
}

// --- Poll-based state machines ---

pub struct PrimevBuildersFetch {
    req: PendingResponse,
}

impl PrimevBuildersFetch {
    pub fn new(http_client: &HttpClient, config: &PrimevConfig) -> Option<Self> {
        let topic = bls_key_added_topic();
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"eth_getLogs","params":[{{"fromBlock":"0x0","address":"{}","topics":["{}"]}}]}}"#,
            config.builder_contract, topic
        );
        let url = Url::parse(&config.builder_url).ok()?;
        let req = http_client.post(&url, Bytes::from(body)).ok()?;
        Some(Self { req })
    }

    pub fn poll(&mut self) -> Poll<Vec<BlsPublicKeyBytes>> {
        match self.req.poll_json::<EthJsonRpcResult<Vec<EthLog>>>() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                error!(%e, "primev builders fetch error");
                Poll::Ready(vec![])
            }
            Poll::Ready(Ok(resp)) => {
                let keys = resp
                    .result
                    .iter()
                    .filter_map(|log| {
                        let data = alloy_primitives::hex::decode(log.data.trim_start_matches("0x"))
                            .ok()?;
                        process_bls_key_data(&data)
                    })
                    .collect();
                Poll::Ready(keys)
            }
        }
    }
}

pub struct PrimevValidatorsFetch {
    req: PendingResponse,
    func: ethers::abi::Function,
    duties: Vec<ProposerDuty>,
}

impl PrimevValidatorsFetch {
    pub fn new(
        http_client: &HttpClient,
        config: &PrimevConfig,
        duties: Vec<ProposerDuty>,
    ) -> Option<Self> {
        if duties.is_empty() {
            return None;
        }
        let abi: Abi = serde_json::from_str(VALIDATOR_ABI).ok()?;
        let func = abi.function("areValidatorsOptedIn").ok()?.clone();
        let pubkeys: Vec<Token> =
            duties.iter().map(|d| Token::Bytes(d.pubkey.0.to_vec())).collect();
        let call_data = func.encode_input(&[Token::Array(pubkeys)]).ok()?;
        let hex_data = format!("0x{}", alloy_primitives::hex::encode(&call_data));
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{{"to":"{}","data":"{}"}},"latest"]}}"#,
            config.validator_contract, hex_data
        );
        let url = Url::parse(&config.validator_url).ok()?;
        let req = http_client.post(&url, Bytes::from(body)).ok()?;
        Some(Self { req, func, duties })
    }

    pub fn poll(&mut self) -> Poll<Vec<BlsPublicKeyBytes>> {
        match self.req.poll_json::<EthJsonRpcResult<String>>() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                error!(%e, "primev validators fetch error");
                Poll::Ready(vec![])
            }
            Poll::Ready(Ok(resp)) => {
                let result_bytes =
                    match alloy_primitives::hex::decode(resp.result.trim_start_matches("0x")) {
                        Ok(b) => b,
                        Err(e) => {
                            error!(%e, "validators result hex decode");
                            return Poll::Ready(vec![]);
                        }
                    };
                let decoded = match self.func.decode_output(&result_bytes) {
                    Ok(d) => d,
                    Err(e) => {
                        error!(%e, "validators ABI decode");
                        return Poll::Ready(vec![]);
                    }
                };
                Poll::Ready(extract_opted_in(&decoded, &self.duties))
            }
        }
    }
}

fn extract_opted_in(decoded: &[Token], duties: &[ProposerDuty]) -> Vec<BlsPublicKeyBytes> {
    let tuples = match decoded.first() {
        Some(Token::Array(v)) => v,
        _ => return vec![],
    };
    tuples
        .iter()
        .enumerate()
        .filter_map(|(i, token)| {
            if let Token::Tuple(values) = token {
                let opted_in = values.iter().any(|t| matches!(t, Token::Bool(true)));
                if opted_in { duties.get(i).map(|d| d.pubkey) } else { None }
            } else {
                None
            }
        })
        .collect()
}

// --- Result processing ---

pub fn build_primev_builder_configs(
    primev_builders: Vec<BlsPublicKeyBytes>,
    local_cache: &LocalCache,
) -> Vec<BuilderConfig> {
    let mut configs = Vec::new();
    for builder_pubkey in primev_builders {
        match local_cache.get_builder_info(&builder_pubkey) {
            Some(builder_info) => {
                if builder_info.builder_id.as_deref() == Some(PRIMEV_BUILDER_ID) ||
                    builder_info
                        .builder_ids
                        .as_ref()
                        .is_some_and(|v| v.iter().any(|s| s == PRIMEV_BUILDER_ID))
                {
                    continue;
                }
                let builder_ids = match builder_info.builder_ids {
                    Some(mut ids) => {
                        if !ids.iter().any(|s| s == PRIMEV_BUILDER_ID) {
                            ids.push(PRIMEV_BUILDER_ID.to_string());
                        }
                        Some(ids)
                    }
                    None => Some(vec![PRIMEV_BUILDER_ID.to_string()]),
                };
                configs.push(BuilderConfig {
                    pub_key: builder_pubkey,
                    builder_info: BuilderInfo {
                        collateral: builder_info.collateral,
                        is_optimistic: builder_info.is_optimistic,
                        is_optimistic_for_regional_filtering: builder_info
                            .is_optimistic_for_regional_filtering,
                        builder_id: builder_info.builder_id.clone(),
                        builder_ids,
                        api_key: None,
                    },
                });
            }
            None => {
                configs.push(BuilderConfig {
                    pub_key: builder_pubkey,
                    builder_info: BuilderInfo {
                        collateral: U256::ZERO,
                        is_optimistic: false,
                        is_optimistic_for_regional_filtering: false,
                        builder_id: Some(PRIMEV_BUILDER_ID.to_string()),
                        builder_ids: Some(vec![PRIMEV_BUILDER_ID.to_string()]),
                        api_key: None,
                    },
                });
            }
        }
    }
    configs
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;
    use helix_common::{BuilderConfig, BuilderInfo, local_cache::LocalCache};
    use helix_types::BlsPublicKeyBytes;

    use super::{PRIMEV_BUILDER_ID, build_primev_builder_configs};

    fn zero_key() -> BlsPublicKeyBytes {
        BlsPublicKeyBytes::default()
    }

    #[test]
    fn new_builder_gets_primev_id() {
        let cache = LocalCache::new();
        let configs = build_primev_builder_configs(vec![zero_key()], &cache);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].builder_info.builder_id.as_deref(), Some(PRIMEV_BUILDER_ID));
    }

    #[test]
    fn existing_builder_already_primev_skipped() {
        let cache = LocalCache::new();
        cache.update_builder_infos(
            &[BuilderConfig {
                pub_key: zero_key(),
                builder_info: BuilderInfo {
                    builder_id: Some(PRIMEV_BUILDER_ID.to_string()),
                    ..Default::default()
                },
            }],
            false,
        );
        let configs = build_primev_builder_configs(vec![zero_key()], &cache);
        assert!(configs.is_empty());
    }

    #[test]
    fn existing_builder_gets_primev_id_appended() {
        let cache = LocalCache::new();
        cache.update_builder_infos(
            &[BuilderConfig {
                pub_key: zero_key(),
                builder_info: BuilderInfo {
                    builder_id: Some("other".to_string()),
                    collateral: U256::from(100u64),
                    is_optimistic: true,
                    ..Default::default()
                },
            }],
            false,
        );
        let configs = build_primev_builder_configs(vec![zero_key()], &cache);
        assert_eq!(configs.len(), 1);
        let ids = configs[0].builder_info.builder_ids.as_ref().unwrap();
        assert!(ids.iter().any(|s| s == PRIMEV_BUILDER_ID));
        assert_eq!(configs[0].builder_info.collateral, U256::from(100u64));
        assert!(configs[0].builder_info.is_optimistic);
    }
}
