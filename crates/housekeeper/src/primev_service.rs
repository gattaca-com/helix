use ethereum_consensus::primitives::BlsPublicKey;
use ethers::{
    abi::{Abi, AbiParser, Address, Bytes},
    contract::{Contract, EthEvent},
    providers::{Http, Provider},
    types::U256,
};
use std::convert::TryFrom;
use std::sync::{Arc, Mutex};
use tracing::{debug, error};
use helix_common::{PrimevConfig, ProposerDuty};
use async_trait::async_trait;

/// Service for interacting with Primev contracts
#[async_trait]
pub trait PrimevService: Send + Sync + 'static {
    /// Fetch validators that have opted into Primev services
    async fn get_registered_primev_validators(
        &self, 
        config: &PrimevConfig, 
        proposer_duties: Vec<ProposerDuty>
    ) -> Vec<BlsPublicKey>;
    
    /// Fetch builders registered with Primev
    async fn get_registered_primev_builders(
        &self, 
        config: &PrimevConfig
    ) -> Vec<BlsPublicKey>;
}

/// Default implementation for Primev service that connects to Ethereum contracts
pub struct EthereumPrimevService;

impl Default for EthereumPrimevService {
    fn default() -> Self {
        Self
    }
}

#[derive(Debug, EthEvent)]
#[ethevent(
    abi = "ProviderRegistered(address indexed provider, uint256 stakedAmount, bytes blsPublicKey)"
)]
pub struct ValueChanged {
    #[ethevent(indexed, name = "provider")]
    pub provider: Address,
    #[ethevent(name = "stakedAmount")]
    pub staked_amount: U256,
    #[ethevent(name = "blsPublicKey")]
    pub bls_public_key: Vec<u8>,
}

#[async_trait]
impl PrimevService for EthereumPrimevService {
    async fn get_registered_primev_builders(
        &self, 
        config: &PrimevConfig
    ) -> Vec<BlsPublicKey> {
        let provider = Provider::<Http>::try_from(config.builder_url.as_str()).unwrap();
        let provider = Arc::new(provider);

        // Define the contract address and ABI
        let provider_registry_address: Address = config.builder_contract.as_str().parse().unwrap();

        let abi_human_readable = r#"
        [
            "event ProviderRegistered(address indexed provider, uint256 stakedAmount, bytes blsPublicKey)"
        ]
        "#;

        // Parse the ABI
        let abi: Abi = AbiParser::default().parse_str(abi_human_readable).unwrap();

        // Create a new contract instance
        let contract = Contract::new(provider_registry_address, abi, provider.clone());

        let event = contract
            .event_for_name("ProviderRegistered")
            .unwrap()
            .from_block(0)
            .address(provider_registry_address.into());

        let providers: Vec<ValueChanged> = match event.query().await {
            Ok(providers) => providers,
            Err(err) => {
                error!("Error querying ProviderRegistered events: {:?}", err);
                Vec::new()
            }
        };

        let mut bls_public_keys = Vec::new();
        for i in providers.iter() {
            bls_public_keys.push(BlsPublicKey::try_from(i.bls_public_key.as_slice()).unwrap());
        }
        bls_public_keys
    }
    
    async fn get_registered_primev_validators(
        &self, 
        config: &PrimevConfig, 
        proposer_duties: Vec<ProposerDuty>
    ) -> Vec<BlsPublicKey> {
        let provider = Provider::<Http>::try_from(config.validator_url.as_str()).unwrap();
        let provider = Arc::new(provider);

        // Define the contract address and ABI
        let validator_registry_address: Address = config.validator_contract.as_str().parse().unwrap();

        // The abi is specific to the validatorOptedInRouter contract
        // see https://docs.primev.xyz/v1.0.0/developers/mainnet#l1-validator-registries for contract details
        let abi_str = r#"[
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

        let abi: Abi = serde_json::from_str(abi_str).unwrap();

        // Create a new contract instance
        let contract = Contract::new(validator_registry_address, abi, provider.clone());

        let validator_pubkeys: Vec<Bytes> = proposer_duties
            .iter()
            .map(|duty| {
                // Convert BlsPublicKey to Bytes for the contract call
                Bytes::from(duty.public_key.to_vec())
            })
            .collect();

        // Call the areValidatorsOptedIn function to check which validators are opted in
        let opted_in_statuses = match contract
            .method::<_, Vec<(bool, bool, bool)>>("areValidatorsOptedIn", (validator_pubkeys,))
        {
            Ok(method) => match method.call().await {
                Ok(statuses) => {
                    debug!("Successfully queried opt-in status for {} validators", statuses.len());
                    statuses
                },
                Err(e) => {
                    error!("Error calling areValidatorsOptedIn: {:?}", e);
                    Vec::new()
                }
            },
            Err(e) => {
                error!("Error creating areValidatorsOptedIn method call: {:?}", e);
                Vec::new()
            }
        };

        // Extract the public keys of validators that are opted into any Primev service
        let mut opted_in_validators = Vec::new();
        for (index, status) in opted_in_statuses.iter().enumerate() {
            // A validator is considered opted in if any of the three flags is true
            // (isVanillaOptedIn || isAvsOptedIn || isMiddlewareOptedIn)
            if status.0 || status.1 || status.2 {
                if let Some(duty) = proposer_duties.get(index) {
                    opted_in_validators.push(duty.public_key.clone());
                }
            }
        }

        // Return just the list of opted-in validator pubkeys
        opted_in_validators
    }
}
#[cfg(test)]
pub struct MockPrimevService {
    pub mock_validators: Vec<BlsPublicKey>,
    pub mock_builders: Vec<BlsPublicKey>,
    operation_tracker: Option<Arc<Mutex<Vec<&'static str>>>>,
}

#[cfg(test)]
impl MockPrimevService {
    pub fn new() -> Self {
        Self {
            mock_validators: Vec::new(),
            mock_builders: Vec::new(),
            operation_tracker: None,  // Initialize as None
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
    async fn get_registered_primev_validators(
        &self, 
        _config: &PrimevConfig, 
        _proposer_duties: Vec<ProposerDuty>
    ) -> Vec<BlsPublicKey> {
        if let Some(tracker) = &self.operation_tracker {
            tracker.lock().unwrap().push("get_registered_primev_validators");
        }
        self.mock_validators.clone()
    }
    
    async fn get_registered_primev_builders(
        &self, 
        _config: &PrimevConfig
    ) -> Vec<BlsPublicKey> {
        if let Some(tracker) = &self.operation_tracker {
            tracker.lock().unwrap().push("get_registered_primev_builders");
        }
        println!("get_registered_primev_builders: {:?}", self.mock_builders);
        self.mock_builders.clone()
    }
}