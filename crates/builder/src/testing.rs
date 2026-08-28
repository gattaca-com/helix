use std::str::FromStr;

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use ethrex_common::types::Genesis;
use ethrex_config::networks::Network;
use ethrex_storage::{EngineType, Store};

use crate::engine::convert::eaddr;

/// Dev keys from ethrex's `fixtures/keys/private_keys_l1.txt`; a fixture picks
/// the ones the `LocalDevnet` genesis actually funds.
pub const KEYS: [&str; 20] = [
    "0x941e103320615d394a55708be13e45994c7d93b932b064dbcb2b511fe3254e2e",
    "0xbcdf20249abf0ed6d944c0288fad489e33f66b3960d9e6229c1cd214ed3bbe31",
    "0x39725efee3fb28614de3bacaffe4cc4bd8c436257e2c8bb887c4b5c4be45e76d",
    "0x53321db7c1e331d93a11a41d16f004d7ff63972ec8ec7c25db329728ceeb1710",
    "0xab63b23eb7941c1251757e24b3d2350d2bc05c3c388d06f8fe6feafefb1e8c70",
    "0x5d2344259f42259f82d2c140aa66102ba89b57b4883ee441a8b312622bd42491",
    "0x27515f805127bebad2fb9b183508bdacb8c763da16f54e0678b16e8f28ef3fff",
    "0x7ff1a4c1d57e5e784d327c4c7651e952350bc271f156afb3d00d20f5ef924856",
    "0x3a91003acaf4c21b3953d94fa4a6db694fa69e5242b2e37be05dd82761058899",
    "0xbb1d0f125b4fb2bb173c318cdead45468474ca71474e2247776b2b4c0fa2d3f5",
    "0x850643a0224065ecce3882673c21f56bcf6eef86274cc21cadff15930b59fc8c",
    "0x94eb3102993b41ec55c241060f47daa0f6372e2e3ad7e91612ae36c364042e44",
    "0xdaf15504c22a352648a71ef2926334fe040ac1d5005019e09f6c979808024dc7",
    "0xeaba42282ad33c8ef2524f07277c03a776d98ae19f581990ce75becb7cfa1c23",
    "0x3fd98b5187bf6526734efaa644ffbb4e3670d66f5d0268ce0323ec09124bff61",
    "0x5288e2f440c7f0cb61a9be8afdeb4295f786383f96f5e35eb0c94ef103996b64",
    "0xf296c7802555da2a5a662be70e078cbd38b44f96f8615ae529da41122ce8db05",
    "0xbf3beef3bd999ba9f2451e06936f0423cd62b815c9233dd3bc90f7e02a1e8673",
    "0x6ecadc396415970e91293726c3f5775225440ea0844ae5616135fd10d66b5954",
    "0xa492823c3e193d6c595f37a18e3c06650cf4c74558cc818b16130b293716106f",
];

pub const GWEI: u128 = 1_000_000_000;
pub const ETH: u128 = 1_000_000_000_000_000_000;

#[allow(clippy::too_many_arguments)]
pub fn signed_transfer(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Vec<u8> {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to: to.into(),
        value,
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    alloy_consensus::TxEnvelope::from(tx.into_signed(signature)).encoded_2718()
}

pub async fn dev_genesis_store() -> (Store, Genesis) {
    let genesis = Network::LocalDevnet.get_genesis().unwrap();
    let mut store = Store::new("memory", EngineType::InMemory).unwrap();
    store.add_initial_state(genesis.clone()).await.unwrap();
    (store, genesis)
}

pub fn funded_signers(genesis: &Genesis, count: usize) -> Vec<PrivateKeySigner> {
    let signers: Vec<PrivateKeySigner> = KEYS
        .iter()
        .map(|key| PrivateKeySigner::from_str(key).unwrap())
        .filter(|signer| genesis.alloc.contains_key(&eaddr(signer.address())))
        .take(count)
        .collect();
    assert_eq!(signers.len(), count, "dev genesis funds too few of the fixture keys");
    signers
}
