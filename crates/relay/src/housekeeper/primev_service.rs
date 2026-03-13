use std::{convert::TryFrom, sync::Arc};

use ethers::{
    abi::{Abi, AbiParser, Address, Bytes},
    contract::{Contract, EthEvent},
    providers::{Http, Middleware, Provider},
    types::transaction::eip2718::TypedTransaction,
};
use helix_common::{PrimevConfig, ProposerDuty};
use helix_types::BlsPublicKeyBytes;
use tracing::{debug, error};

#[derive(Debug, EthEvent)]
#[ethevent(abi = "BLSKeyAdded(address indexed provider, bytes blsPublicKey)")]
pub struct ValueChanged {
    #[ethevent(indexed, name = "provider")]
    pub provider: Address,
    #[ethevent(name = "blsPublicKey")]
    pub bls_public_key: Vec<u8>,
}

/// Helper function to process BLS keys from raw event data
fn process_bls_key_data(data: &[u8]) -> Option<BlsPublicKeyBytes> {
    debug!(raw = alloy_primitives::hex::encode_prefixed(data), "Raw BLS key data");

    // Try directly with the raw data first
    if let Ok(key) = BlsPublicKeyBytes::try_from(data) {
        return Some(key);
    }

    // Remove the Solidity encoding overhead similar to the jq command
    // The pattern is typically:
    // - First 32 bytes (0x00...0020) indicate offset to data
    // - Next 32 bytes indicate length of the data
    // - Remaining bytes are the actual data

    if data.len() < 64 {
        debug!("Data too short for BLS key");
        return None;
    }

    // Extract the data part after the length prefix
    let data_part = &data[64..];
    if data_part.len() < 48 {
        debug!("Data part too short for BLS key: {}", data_part.len());
        return None;
    }

    // Use only the first 48 bytes which is the BLS pubkey size
    let key_bytes = &data_part[..48];

    // Extract the BLS key
    match BlsPublicKeyBytes::try_from(key_bytes) {
        Ok(key) => Some(key),
        Err(err) => {
            debug!("Failed to create BLS key from processed data: {:?}", err);
            None
        }
    }
}

/// Default implementation for Primev service that connects to Ethereum contracts
#[derive(Clone)]
pub struct EthereumPrimevService {
    builder_contract: Contract<Provider<Http>>,
    validator_contract: Contract<Provider<Http>>,
}

impl EthereumPrimevService {
    /// Create a new EthereumPrimevService with the given configuration
    /// This ensures the service is always initialized before use
    pub fn new(config: PrimevConfig) -> eyre::Result<Self> {
        // Initialize builder contract
        let builder_provider = Provider::<Http>::try_from(config.builder_url.as_str())?;
        let builder_provider = Arc::new(builder_provider);
        let builder_address: Address = config.builder_contract.as_str().parse()?;

        // Builder contract setup
        let abi_human_readable = r#"
        [
            "event BLSKeyAdded(address indexed provider, bytes blsPublicKey)"
        ]
        "#;
        let builder_abi = AbiParser::default().parse_str(abi_human_readable)?;
        let builder_contract = Contract::new(builder_address, builder_abi, builder_provider);

        // Initialize validator contract
        let validator_provider = Provider::<Http>::try_from(config.validator_url.as_str())?;
        let validator_provider = Arc::new(validator_provider);
        let validator_address: Address = config.validator_contract.as_str().parse()?;

        // Validator contract setup
        let validator_abi_str = r#"[
        {
            "type": "function",
            "name": "areValidatorsOptedIn",
            "inputs": [
            {
                "name": "valBLSPubKeys",
                "type": "bytes[]",
                "internalType": "bytes[]"
            }
            ],
            "outputs": [
            {
                "name": "",
                "type": "tuple[]",
                "internalType": "struct IValidatorOptInRouter.OptInStatus[]",
                "components": [
                {
                    "name": "isVanillaOptedIn",
                    "type": "bool",
                    "internalType": "bool"
                },
                {
                    "name": "isAvsOptedIn",
                    "type": "bool",
                    "internalType": "bool"
                },
                {
                    "name": "isMiddlewareOptedIn",
                    "type": "bool",
                    "internalType": "bool"
                }
                ]
            }
            ],
            "stateMutability": "view"
        }
        ]"#;
        let validator_abi: Abi = serde_json::from_str(validator_abi_str)?;
        let validator_contract =
            Contract::new(validator_address, validator_abi, validator_provider);

        Ok(Self { builder_contract, validator_contract })
    }
}

impl EthereumPrimevService {
    pub async fn get_registered_primev_builders(&self) -> Vec<BlsPublicKeyBytes> {
        let event = self.builder_contract.event_for_name("BLSKeyAdded").unwrap().from_block(0);

        let providers: Vec<ValueChanged> = match event.query().await {
            Ok(providers) => providers,
            Err(err) => {
                error!("Error querying BLSKeyAdded events: {:?}", err);
                Vec::new()
            }
        };

        let mut result = Vec::new();
        for (i, value) in providers.iter().enumerate() {
            if let Some(key) = process_bls_key_data(&value.bls_public_key) {
                result.push(key);
            } else {
                error!("Failed to extract BLS key from event {}", i);
            }
        }

        result
    }

    pub async fn get_registered_primev_validators(
        &self,
        proposer_duties: Vec<ProposerDuty>,
    ) -> Vec<BlsPublicKeyBytes> {
        if proposer_duties.is_empty() {
            debug!("No proposer duties provided, skipping validator check");
            return Vec::new();
        }

        let validator_pubkeys: Vec<Bytes> =
            proposer_duties.iter().map(|duty| Bytes::from(duty.pubkey.0)).collect();

        let func = match self.validator_contract.abi().function("areValidatorsOptedIn") {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to get function from ABI: {:?}", e);
                return Vec::new();
            }
        };

        let input_tokens = vec![ethers::abi::Token::Array(
            validator_pubkeys.iter().map(|key| ethers::abi::Token::Bytes(key.to_vec())).collect(),
        )];

        let call_data = match func.encode_input(&input_tokens) {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to encode function input: {:?}", e);
                return Vec::new();
            }
        };

        let provider = self.validator_contract.client();

        let tx: TypedTransaction = ethers::types::TransactionRequest::new()
            .to(self.validator_contract.address())
            .data(call_data)
            .into();

        let result = match provider.call(&tx, None).await {
            Ok(data) => data,
            Err(e) => {
                error!("Contract call failed: {:?}", e);
                return Vec::new();
            }
        };

        let decoded = match func.decode_output(&result) {
            Ok(tokens) => tokens,
            Err(e) => {
                error!("Failed to decode output: {:?}", e);
                return Vec::new();
            }
        };

        // Convert the decoded output to the expected Vec<(bool, bool, bool)> format
        let opted_in_statuses = if let Some(ethers::abi::Token::Array(tuples)) = decoded.first() {
            tuples
                .iter()
                .map(|token| {
                    if let ethers::abi::Token::Tuple(values) = token &&
                        values.len() >= 3 &&
                        let (
                            ethers::abi::Token::Bool(vanilla_opted_in),
                            ethers::abi::Token::Bool(avs_opted_in),
                            ethers::abi::Token::Bool(middleware_opted_in),
                        ) = (
                            values.first().unwrap_or(&ethers::abi::Token::Bool(false)),
                            values.get(1).unwrap_or(&ethers::abi::Token::Bool(false)),
                            values.get(2).unwrap_or(&ethers::abi::Token::Bool(false)),
                        )
                    {
                        return (*vanilla_opted_in, *avs_opted_in, *middleware_opted_in);
                    }
                    (false, false, false) // Default if parsing fails
                })
                .collect()
        } else {
            // Return empty Vec if output doesn't match expected format
            Vec::new()
        };

        // Extract the public keys of validators that are opted into any Primev service
        let mut opted_in_validators = Vec::new();
        for (index, status) in opted_in_statuses.iter().enumerate() {
            if (status.0 || status.1 || status.2) &&
                let Some(duty) = proposer_duties.get(index)
            {
                opted_in_validators.push(duty.pubkey);
            }
        }

        opted_in_validators
    }
}
