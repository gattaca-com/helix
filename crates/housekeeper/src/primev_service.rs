#[cfg(test)]
use std::sync::Mutex;
use std::{convert::TryFrom, sync::Arc};

use alloy_rpc_types::beacon::BlsPublicKey;
use async_trait::async_trait;
use ethers::{
    abi::{Abi, AbiParser, Address, Bytes},
    contract::{Contract, EthEvent},
    providers::{Http, Middleware, Provider},
    types::transaction::eip2718::TypedTransaction,
};
use helix_common::{PrimevConfig, ProposerDuty};
use tracing::{debug, error};

/// Service for interacting with Primev contracts
#[async_trait]
pub trait PrimevService: Send + Sync + 'static {
    /// Initialize the service with configuration
    async fn initialize(
        &mut self,
        config: PrimevConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Fetch validators that have opted into Primev services
    async fn get_registered_primev_validators(
        &self,
        proposer_duties: Vec<ProposerDuty>,
    ) -> Vec<BlsPublicKey>;

    /// Fetch builders registered with Primev
    async fn get_registered_primev_builders(&self) -> Vec<BlsPublicKey>;
}

#[derive(Debug, EthEvent)]
#[ethevent(abi = "BLSKeyAdded(address indexed provider, bytes blsPublicKey)")]
pub struct ValueChanged {
    #[ethevent(indexed, name = "provider")]
    pub provider: Address,
    #[ethevent(name = "blsPublicKey")]
    pub bls_public_key: Vec<u8>,
}

/// Helper function to process BLS keys from raw event data
fn process_bls_key_data(data: &[u8]) -> Option<BlsPublicKey> {
    debug!(raw = alloy_primitives::hex::encode_prefixed(data), "Raw BLS key data");

    // Try directly with the raw data first
    if let Ok(key) = BlsPublicKey::try_from(data) {
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
    match BlsPublicKey::try_from(key_bytes) {
        Ok(key) => Some(key),
        Err(err) => {
            debug!("Failed to create BLS key from processed data: {:?}", err);
            None
        }
    }
}

/// Default implementation for Primev service that connects to Ethereum contracts
pub struct EthereumPrimevService {
    builder_contract: Contract<Provider<Http>>,
    validator_contract: Contract<Provider<Http>>,
}

impl EthereumPrimevService {
    /// Create a new EthereumPrimevService with the given configuration
    /// This ensures the service is always initialized before use
    pub async fn new(
        config: PrimevConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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

    /// Create an uninitialized service - only for compatibility with the PrimevService trait
    fn uninitialized() -> Self {
        panic!("EthereumPrimevService must be initialized with new() - cannot create uninitialized instance")
    }
}

// We need to keep this implementation for the trait, but we'll make it panic
// if someone tries to use it, forcing them to use new() instead
#[async_trait]
impl PrimevService for EthereumPrimevService {
    async fn initialize(
        &mut self,
        _config: PrimevConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // This method exists only for trait compatibility
        // It should never be called directly on EthereumPrimevService
        panic!("EthereumPrimevService must be initialized with new() method, not through the trait")
    }

    async fn get_registered_primev_builders(&self) -> Vec<BlsPublicKey> {
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

    async fn get_registered_primev_validators(
        &self,
        proposer_duties: Vec<ProposerDuty>,
    ) -> Vec<BlsPublicKey> {
        if proposer_duties.is_empty() {
            debug!("No proposer duties provided, skipping validator check");
            return Vec::new();
        }

        let validator_pubkeys: Vec<Bytes> =
            proposer_duties.iter().map(|duty| Bytes::from(duty.public_key.to_vec())).collect();

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
                    if let ethers::abi::Token::Tuple(values) = token {
                        if values.len() >= 3 {
                            if let (
                                ethers::abi::Token::Bool(vanilla_opted_in),
                                ethers::abi::Token::Bool(avs_opted_in),
                                ethers::abi::Token::Bool(middleware_opted_in),
                            ) = (
                                values.first().unwrap_or(&ethers::abi::Token::Bool(false)),
                                values.get(1).unwrap_or(&ethers::abi::Token::Bool(false)),
                                values.get(2).unwrap_or(&ethers::abi::Token::Bool(false)),
                            ) {
                                return (*vanilla_opted_in, *avs_opted_in, *middleware_opted_in);
                            }
                        }
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
            if status.0 || status.1 || status.2 {
                if let Some(duty) = proposer_duties.get(index) {
                    opted_in_validators.push(duty.public_key);
                }
            }
        }

        opted_in_validators
    }
}

// Implement Default for API compatibility, but make it clear it should not be used
impl Default for EthereumPrimevService {
    fn default() -> Self {
        // For API compatibility only, will panic if used
        Self::uninitialized()
    }
}

#[cfg(test)]
pub struct MockPrimevService {
    pub mock_validators: Vec<BlsPublicKey>,
    pub mock_builders: Vec<BlsPublicKey>,
    operation_tracker: Option<Arc<Mutex<Vec<&'static str>>>>,
    is_initialized: bool,
}

#[cfg(test)]
impl MockPrimevService {
    pub fn new() -> Self {
        // Create default test values
        let default_validator = BlsPublicKey::try_from(vec![1; 48].as_slice()).unwrap_or_default();
        let default_builder = BlsPublicKey::try_from(vec![2; 48].as_slice()).unwrap_or_default();

        Self {
            mock_validators: vec![default_validator],
            mock_builders: vec![default_builder],
            operation_tracker: None,
            is_initialized: false,
        }
    }

    pub fn with_validators(mut self, validators: Vec<BlsPublicKey>) -> Self {
        self.mock_validators = validators;
        self
    }

    pub fn with_builders(mut self, builders: Vec<BlsPublicKey>) -> Self {
        self.mock_builders = builders;
        self
    }

    pub fn set_operation_tracker(&mut self, tracker: Arc<Mutex<Vec<&'static str>>>) {
        self.operation_tracker = Some(tracker);
    }
}

#[cfg(test)]
#[async_trait]
impl PrimevService for MockPrimevService {
    async fn initialize(
        &mut self,
        _config: PrimevConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(tracker) = &self.operation_tracker {
            tracker.lock().unwrap().push("initialize");
        }
        self.is_initialized = true;
        Ok(())
    }

    async fn get_registered_primev_validators(
        &self,
        _proposer_duties: Vec<ProposerDuty>,
    ) -> Vec<BlsPublicKey> {
        if let Some(tracker) = &self.operation_tracker {
            tracker.lock().unwrap().push("get_registered_primev_validators");
        }

        if !self.is_initialized {
            debug!("Using default mock validators because service not initialized");
        }

        self.mock_validators.clone()
    }

    async fn get_registered_primev_builders(&self) -> Vec<BlsPublicKey> {
        if let Some(tracker) = &self.operation_tracker {
            tracker.lock().unwrap().push("get_registered_primev_builders");
        }

        if !self.is_initialized {
            debug!("Using default mock builders because service not initialized");
        }

        self.mock_builders.clone()
    }
}
