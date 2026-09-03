use std::str::FromStr;

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use ethrex_common::types::{Genesis, GenesisAccount};
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
    dev_genesis_store_with(|_| {}).await
}

/// `edit` may add allocations before the genesis state root is computed.
pub async fn dev_genesis_store_with(edit: impl FnOnce(&mut Genesis)) -> (Store, Genesis) {
    let mut genesis = Network::LocalDevnet.get_genesis().unwrap();
    edit(&mut genesis);
    let mut store = Store::new("memory", EngineType::InMemory).unwrap();
    store.add_initial_state(genesis.clone()).await.unwrap();
    (store, genesis)
}

/// The `PaymentForwarder` runtime, from contracts/README.md.
pub fn deploy_payment_forwarder(genesis: &mut Genesis) {
    genesis.alloc.insert(eaddr(helix_common::PAYMENT_FORWARDER), GenesisAccount {
        code: hex::decode("5f358060e01c4218600f5760401cff5b5f5ffd00").unwrap().into(),
        storage: Default::default(),
        balance: ethrex_common::U256::zero(),
        nonce: 0,
    });
}

/// The EIP-8282 builder deposit and exit predeploys, from ethrex's
/// `fixtures/genesis/l1-bal.json`. Empty code at either address invalidates
/// every Amsterdam block, so an Amsterdam fixture cannot build without them.
pub fn deploy_amsterdam_predeploys(genesis: &mut Genesis) {
    for (address, code) in [
        ("0000884d2aa32eaa155f59a2f24efa73d9008282", BUILDER_DEPOSIT_CODE),
        ("000014574a74c805590aff9499fc7a690f008282", BUILDER_EXIT_CODE),
    ] {
        genesis.alloc.insert(
            ethrex_common::Address::from_slice(&hex::decode(address).unwrap()),
            GenesisAccount {
                code: hex::decode(code).unwrap().into(),
                storage: Default::default(),
                balance: ethrex_common::U256::zero(),
                nonce: 0,
            },
        );
    }
}

const BUILDER_DEPOSIT_CODE: &str = "3373fffffffffffffffffffffffffffffffffffffffe146101065760115f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff1461023457600182026001905f5b5f82111560695781019083028483029004916001019190604e565b90939004925050503660b814608957366102345734610234575f5260205ff35b8034106102345760383567ffffffffffffffff1680633b9aca001161023457633b9aca00029034031061023457600154600101600155600354806006026004015f358155600101602035815560010160403581556001016060358155600101608035815560010160a035905560b85f5f3760b85fa0600101600355005b600354600254808203806101001161011d57506101005b5f5b8181146101c3578281016006026004018160b8028154815260200181600101548152602001816002015480825260401c67ffffffffffffffff16816010018160381c81600701538160301c81600601538160281c81600501538160201c81600401538160181c81600301538160101c81600201538160081c81600101535360200181600301548152602001816004015481526020019060050154905260010161011f565b91018092146101d557906002556101e0565b90505f6002555f6003555b5f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff141561020d57505f5b6001546020828201116102225750505f610228565b01602090035b5f555f60015560b8025ff35b5f5ffd";

const BUILDER_EXIT_CODE: &str = "3373fffffffffffffffffffffffffffffffffffffffe1460cb5760115f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff1461018857600182026001905f5b5f82111560685781019083028483029004916001019190604d565b909390049250505036603014608857366101885734610188575f5260205ff35b341061018857600154600101600155600354806003026004013381556001015f35815560010160203590553360601b5f5260305f60143760445fa0600101600355005b6003546002548082038060101160df575060105b5f5b8181146101175782810160030260040181604402815460601b8152601401816001015481526020019060020154905260010160e1565b91018092146101295790600255610134565b90505f6002555f6003555b5f54807fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff141561016157505f5b6001546002828201116101765750505f61017c565b01600290035b5f555f6001556044025ff35b5f5ffd";

/// Runtime that reads the balance of the address in its calldata and stops:
/// PUSH0 CALLDATALOAD BALANCE POP STOP. Reads an account without touching it.
pub fn deploy_balance_probe(genesis: &mut Genesis, address: Address) {
    genesis.alloc.insert(eaddr(address), GenesisAccount {
        code: hex::decode("5f35315000").unwrap().into(),
        storage: Default::default(),
        balance: ethrex_common::U256::zero(),
        nonce: 0,
    });
}

/// A legacy transaction with no chain id, which EIP-155 replay protection does
/// not cover.
pub fn signed_unprotected_transfer(
    signer: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    value: U256,
    gas_price: u128,
) -> Vec<u8> {
    let tx = alloy_consensus::TxLegacy {
        chain_id: None,
        nonce,
        gas_price,
        gas_limit: 21_000,
        to: to.into(),
        value,
        input: Default::default(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    alloy_consensus::TxEnvelope::from(tx.into_signed(signature)).encoded_2718()
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

/// Blobs with real commitments and EIP-7594 cell proofs. Each blob differs, so
/// no two commitments collide.
pub fn blob_bundle(count: usize) -> ethrex_common::types::BlobsBundle {
    let blobs = (0..count)
        .map(|i| {
            let mut blob = [0u8; ethrex_common::types::BYTES_PER_BLOB];
            blob[0] = i as u8 + 1;
            blob
        })
        .collect();
    ethrex_common::types::BlobsBundle::create_from_blobs(&blobs, Some(1)).unwrap()
}

#[allow(clippy::too_many_arguments)]
pub fn signed_blob_transfer(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: Address,
    versioned_hashes: Vec<alloy_primitives::B256>,
) -> Vec<u8> {
    let tx = alloy_consensus::TxEip4844 {
        chain_id,
        nonce,
        gas_limit: 100_000,
        max_fee_per_gas: 100_000_000_000,
        max_priority_fee_per_gas: 0,
        to,
        value: U256::ZERO,
        access_list: Default::default(),
        blob_versioned_hashes: versioned_hashes,
        max_fee_per_blob_gas: 1_000_000_000,
        input: Default::default(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    alloy_consensus::TxEnvelope::from(tx.into_signed(signature)).encoded_2718()
}
