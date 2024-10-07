use ethereum_consensus::{
    clock::{
        for_goerli, for_holesky, for_mainnet, for_sepolia, from_system_time, Clock, SystemTimeProvider, GOERLI_GENESIS_TIME, HOLESKY_GENESIS_TIME,
        MAINNET_GENESIS_TIME, SEPOLIA_GENESIS_TIME,
    },
    configs,
    primitives::Root,
    ssz::prelude::*,
    state_transition::Context,
    Error,
};

pub(crate) const MAINNET_GENESIS_VALIDATOR_ROOT: [u8; 32] =
    [75, 54, 61, 185, 78, 40, 97, 32, 215, 110, 185, 5, 52, 15, 221, 78, 84, 191, 233, 240, 107, 243, 63, 246, 207, 90, 210, 127, 81, 27, 254, 149];
pub(crate) const SEPOLIA_GENESIS_VALIDATOR_ROOT: [u8; 32] =
    [216, 234, 23, 31, 60, 148, 174, 162, 30, 188, 66, 161, 237, 97, 5, 42, 207, 63, 146, 9, 192, 14, 78, 251, 170, 221, 172, 9, 237, 155, 128, 120];
pub(crate) const GOERLI_GENESIS_VALIDATOR_ROOT: [u8; 32] =
    [4, 61, 176, 217, 168, 56, 19, 85, 30, 226, 243, 52, 80, 210, 55, 151, 117, 125, 67, 9, 17, 169, 50, 5, 48, 173, 138, 14, 171, 196, 62, 251];
pub(crate) const HOLESKY_GENESIS_VALIDATOR_ROOT: [u8; 32] = [
    145, 67, 170, 124, 97, 90, 127, 113, 21, 226, 182, 170, 195, 25, 192, 53, 41, 223, 130, 66, 174, 112, 95, 186, 157, 243, 155, 121, 197, 159, 168,
    177,
];

#[derive(Default, Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Mainnet,
    Sepolia,
    Goerli,
    Holesky,
    Custom(String),
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mainnet => write!(f, "mainnet"),
            Self::Sepolia => write!(f, "sepolia"),
            Self::Goerli => write!(f, "goerli"),
            Self::Holesky => write!(f, "holesky"),
            Self::Custom(config) => write!(f, "custom network with config at `{config}`"),
        }
    }
}

impl TryFrom<&Network> for Context {
    type Error = Error;

    fn try_from(network: &Network) -> Result<Self, Self::Error> {
        match network {
            Network::Mainnet => Ok(Context::for_mainnet()),
            Network::Sepolia => Ok(Context::for_sepolia()),
            Network::Goerli => Ok(Context::for_goerli()),
            Network::Holesky => Ok(Context::for_holesky()),
            Network::Custom(config) => Context::try_from_file(config),
        }
    }
}

#[derive(Clone)]
pub struct ChainInfo {
    pub network: Network,
    pub genesis_validators_root: Root,
    pub context: Context,
    pub clock: Clock<SystemTimeProvider>,
    pub genesis_time_in_secs: u64,
    pub seconds_per_slot: u64,
}

impl ChainInfo {
    pub fn for_mainnet() -> Self {
        let mut cxt = Context::for_mainnet();
        // override the deneb fork epoch and version as library defaults are incorrect
        // TODO: remove this once the library defaults are fixed
        cxt.deneb_fork_epoch = 269568;

        Self {
            network: Network::Mainnet,
            genesis_validators_root: Node::try_from(MAINNET_GENESIS_VALIDATOR_ROOT.as_ref()).unwrap(),
            context: cxt,
            clock: for_mainnet(),
            genesis_time_in_secs: MAINNET_GENESIS_TIME,
            seconds_per_slot: configs::mainnet::SECONDS_PER_SLOT,
        }
    }

    pub fn for_sepolia() -> Self {
        Self {
            network: Network::Sepolia,
            genesis_validators_root: Node::try_from(SEPOLIA_GENESIS_VALIDATOR_ROOT.as_ref()).unwrap(),
            context: Context::for_sepolia(),
            clock: for_sepolia(),
            genesis_time_in_secs: SEPOLIA_GENESIS_TIME,
            seconds_per_slot: configs::sepolia::SECONDS_PER_SLOT,
        }
    }

    pub fn for_goerli() -> Self {
        Self {
            network: Network::Goerli,
            genesis_validators_root: Node::try_from(GOERLI_GENESIS_VALIDATOR_ROOT.as_ref()).unwrap(),
            context: Context::for_goerli(),
            clock: for_goerli(),
            genesis_time_in_secs: GOERLI_GENESIS_TIME,
            seconds_per_slot: configs::goerli::SECONDS_PER_SLOT,
        }
    }

    pub fn for_holesky() -> Self {
        let mut cxt = Context::for_holesky();
        // override the deneb fork epoch and version as library defaults are incorrect
        // TODO: remove this once the library defaults are fixed
        cxt.deneb_fork_epoch = 29696;
        cxt.deneb_fork_version = [5, 1, 112, 0];

        Self {
            network: Network::Holesky,
            genesis_validators_root: Node::try_from(HOLESKY_GENESIS_VALIDATOR_ROOT.as_ref()).unwrap(),
            context: cxt,
            clock: for_holesky(),
            genesis_time_in_secs: HOLESKY_GENESIS_TIME,
            seconds_per_slot: configs::holesky::SECONDS_PER_SLOT,
        }
    }

    pub fn for_custom(config: String, genesis_validators_root: Node, genesis_time_in_secs: u64) -> Result<Self, Error> {
        let context = Context::try_from_file(&config)?;
        let network = Network::Custom(config.clone());
        let clock = from_system_time(genesis_time_in_secs, context.seconds_per_slot, context.slots_per_epoch);
        let genesis_time_in_secs = genesis_time_in_secs;
        let seconds_per_slot = context.seconds_per_slot;

        Ok(Self { network, genesis_validators_root, context, clock, genesis_time_in_secs, seconds_per_slot })
    }
}
