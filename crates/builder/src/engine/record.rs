use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use dashmap::{DashMap, DashSet};
use ethrex_common::{
    Address, H256, U256,
    types::{AccountState, BlockHeader, ChainConfig, Code, CodeMetadata, GenesisAccount},
};
use ethrex_levm::{db::Database, errors::DatabaseError};
use ethrex_rlp::encode::RLPEncode;
use serde::{Deserialize, Serialize};
use ssz::Encode;

use crate::engine::EngineEvent;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordConfig {
    pub dir: PathBuf,
    /// Record one slot in this many.
    pub every_n_slots: u64,
}

pub const KIND_RELAY_CONFIG: u8 = 0;
pub const KIND_SLOT_START: u8 = 1;
pub const KIND_SLOT_END: u8 = 2;
pub const KIND_MERGEABLE: u8 = 3;
pub const KIND_ACTIVATE: u8 = 4;

/// Signs distributions in replay; a fixture key, so no production key leaves the builder.
pub fn replay_signer() -> alloy_signer_local::PrivateKeySigner {
    "0x941e103320615d394a55708be13e45994c7d93b932b064dbcb2b511fe3254e2e"
        .parse()
        .expect("fixture key")
}

/// Storage slot of `owners[owner]` in a Safe (mapping at slot 2).
pub fn safe_owner_slot(owner: Address) -> H256 {
    let mut preimage = [0u8; 64];
    preimage[12..32].copy_from_slice(owner.as_bytes());
    preimage[63] = 2;
    H256::from(ethrex_crypto::keccak::keccak_hash(preimage))
}

pub const SAFE_SENTINEL: Address = ethrex_common::H160({
    let mut bytes = [0u8; 20];
    bytes[19] = 1;
    bytes
});

/// Encoded trie nodes along every path the slot read, so a replay walks the
/// same mainnet trie shape. Storage nodes are keyed by hashed address.
#[derive(Serialize, Deserialize, Default)]
pub struct TrieProofs {
    pub state: Vec<Vec<u8>>,
    pub storage: std::collections::BTreeMap<H256, Vec<Vec<u8>>>,
}

#[derive(Serialize, Deserialize)]
pub struct RecordMeta {
    pub slot: u64,
    pub relay_signer: ethrex_common::Address,
}

/// One slot's engine input and the parent state it read, enough to replay the
/// slot against an in-memory store.
pub struct SlotRecorder {
    dir: PathBuf,
    slot: u64,
    relay_signer: Address,
    events: Mutex<Vec<u8>>,
    accounts: DashMap<Address, AccountState>,
    absent: DashSet<Address>,
    storage: DashMap<(Address, H256), U256>,
    codes: DashMap<H256, Code>,
    block_hashes: DashMap<u64, H256>,
    /// Per replayed base, each tx with the cumulative gas after it, so a replay
    /// can name the first tx whose execution diverged.
    base_gas: DashMap<alloy_primitives::B256, Vec<(alloy_primitives::B256, u64)>>,
    pub parent_header: OnceLock<BlockHeader>,
    chain_config: OnceLock<ChainConfig>,
    inner: OnceLock<Arc<dyn Database>>,
    pub store: OnceLock<ethrex_storage::Store>,
}

impl SlotRecorder {
    pub fn new(config: &RecordConfig, slot: u64, relay_signer: Address) -> Arc<Self> {
        Arc::new(Self {
            dir: config.dir.join(slot.to_string()),
            slot,
            relay_signer,
            events: Mutex::default(),
            accounts: DashMap::default(),
            absent: DashSet::default(),
            storage: DashMap::default(),
            codes: DashMap::default(),
            block_hashes: DashMap::default(),
            base_gas: DashMap::default(),
            parent_header: OnceLock::new(),
            chain_config: OnceLock::new(),
            inner: OnceLock::new(),
            store: OnceLock::new(),
        })
    }

    pub fn event(&self, recv_ns: u64, event: &EngineEvent) {
        let (kind, body) = match event {
            EngineEvent::RelayConfig(config) => (KIND_RELAY_CONFIG, config.as_ssz_bytes()),
            EngineEvent::SlotStart(msg) => (KIND_SLOT_START, msg.as_ssz_bytes()),
            EngineEvent::SlotEnd { slot } => (KIND_SLOT_END, slot.to_le_bytes().to_vec()),
            EngineEvent::MergeableBlock { body, .. } => (KIND_MERGEABLE, body.clone()),
            EngineEvent::ActivateBase { slot, block_hash, .. } => {
                (KIND_ACTIVATE, [&slot.to_le_bytes()[..], block_hash.as_slice()].concat())
            }
            EngineEvent::ConnectionReset { .. } => return,
        };
        let mut events = self.events.lock().expect("recorder poisoned");
        events.push(kind);
        events.extend_from_slice(&recv_ns.to_le_bytes());
        events.extend_from_slice(&(body.len() as u32).to_le_bytes());
        events.extend_from_slice(&body);
    }

    pub fn base_gas(
        &self,
        base: alloy_primitives::B256,
        txs: impl Iterator<Item = (alloy_primitives::B256, u64)>,
    ) {
        self.base_gas.entry(base).or_insert_with(|| txs.collect());
    }

    /// Wraps a session's parent-state database so its reads land in this recording.
    pub fn wrap(self: &Arc<Self>, inner: Arc<dyn Database>) -> Arc<dyn Database> {
        let _unused = self.inner.set(inner.clone());
        if let Ok(config) = inner.get_chain_config() {
            let _unused = self.chain_config.set(config);
        }
        Arc::new(RecordingDatabase { inner, recorder: self.clone() })
    }

    /// Writes the recording on its own thread; the slot is over, so nothing reads it again.
    pub fn finish(self: Arc<Self>) {
        std::thread::spawn(move || {
            if let Err(err) = self.write() {
                tracing::warn!(slot = self.slot, %err, "slot recording failed");
            }
        });
    }

    fn write(&self) -> std::io::Result<()> {
        let (Some(header), Some(config)) = (self.parent_header.get(), self.chain_config.get())
        else {
            return Ok(());
        };
        std::fs::create_dir_all(&self.dir)?;
        let mut alloc = std::collections::BTreeMap::<Address, GenesisAccount>::new();
        for entry in self.accounts.iter() {
            let code = match self.codes.get(&entry.code_hash) {
                Some(code) => code.code_bytes(),
                None => self
                    .inner
                    .get()
                    .and_then(|inner| inner.get_account_code(entry.code_hash).ok())
                    .map(|code| code.code_bytes())
                    .unwrap_or_default(),
            };
            alloc.insert(*entry.key(), GenesisAccount {
                code,
                storage: Default::default(),
                balance: entry.balance,
                nonce: entry.nonce,
            });
        }
        for entry in self.storage.iter() {
            let (address, key) = *entry.key();
            if let Some(account) = alloc.get_mut(&address) {
                account.storage.insert(U256::from_big_endian(key.as_bytes()), *entry.value());
            }
        }
        let block_hashes: std::collections::BTreeMap<u64, H256> =
            self.block_hashes.iter().map(|entry| (*entry.key(), *entry.value())).collect();
        write_json(&self.dir.join("alloc.json"), &alloc)?;
        write_json(&self.dir.join("chain_config.json"), config)?;
        write_json(&self.dir.join("block_hashes.json"), &block_hashes)?;
        let base_gas: std::collections::BTreeMap<_, _> =
            self.base_gas.iter().map(|entry| (*entry.key(), entry.value().clone())).collect();
        write_json(&self.dir.join("base_gas.json"), &base_gas)?;
        write_json(&self.dir.join("meta.json"), &RecordMeta {
            slot: self.slot,
            relay_signer: self.relay_signer,
        })?;
        std::fs::write(self.dir.join("parent_header.rlp"), header.encode_to_vec())?;
        let accounts: std::collections::BTreeMap<Address, AccountState> =
            self.accounts.iter().map(|entry| (*entry.key(), *entry.value())).collect();
        write_json(&self.dir.join("accounts.json"), &accounts)?;
        {
            let events = self.events.lock().expect("recorder poisoned");
            let mut out =
                zstd::Encoder::new(std::fs::File::create(self.dir.join("events.bin.zst"))?, 3)?;
            out.write_all(&events)?;
            out.finish()?;
        }
        if let Some(store) = self.store.get() {
            let proofs = self.proofs(store, header.state_root).map_err(std::io::Error::other)?;
            let mut out =
                zstd::Encoder::new(std::fs::File::create(self.dir.join("trie.json.zst"))?, 3)?;
            out.write_all(&serde_json::to_vec(&proofs).map_err(std::io::Error::other)?)?;
            out.finish()?;
        }
        Ok(())
    }
}

impl SlotRecorder {
    /// Proofs for every account and slot read, plus the replay signer and the
    /// collateral Safes' owner slots a replay writes to sign distributions.
    fn proofs(
        &self,
        store: &ethrex_storage::Store,
        state_root: H256,
    ) -> Result<TrieProofs, ethrex_storage::error::StoreError> {
        let replay_owner = Address::from_slice(replay_signer().address().as_slice());
        let mut slots: std::collections::BTreeMap<Address, Vec<H256>> = Default::default();
        for entry in self.storage.iter() {
            slots.entry(entry.key().0).or_default().push(entry.key().1);
        }
        for safe in self.collateral_safes() {
            let owner_slots = slots.entry(safe).or_default();
            owner_slots.push(safe_owner_slot(SAFE_SENTINEL));
            owner_slots.push(safe_owner_slot(replay_owner));
        }
        let state = store.open_state_trie(state_root)?;
        let mut nodes = std::collections::BTreeSet::new();
        let addresses =
            self.accounts.iter().map(|entry| *entry.key()).chain(self.absent.iter().map(|a| *a));
        let mut skipped = 0usize;
        for address in addresses.chain([replay_owner]) {
            match proof_with_siblings(&state, &ethrex_storage::hash_address(&address)) {
                Ok(proof) => nodes.extend(proof),
                Err(_) => skipped += 1,
            }
        }
        let mut proofs = TrieProofs { state: nodes.into_iter().collect(), ..Default::default() };
        for (address, keys) in slots {
            let Some(account) = self.accounts.get(&address) else { continue };
            let hashed = H256::from_slice(&ethrex_storage::hash_address(&address));
            let trie = store.open_storage_trie(hashed, state_root, account.storage_root)?;
            let mut nodes = std::collections::BTreeSet::new();
            for key in keys {
                match proof_with_siblings(&trie, &ethrex_storage::hash_key(&key)) {
                    Ok(proof) => nodes.extend(proof),
                    Err(_) => skipped += 1,
                }
            }
            proofs.storage.insert(hashed, nodes.into_iter().collect());
        }
        if skipped > 0 {
            tracing::warn!(slot = self.slot, skipped, "trie proofs skipped for keys");
        }
        Ok(proofs)
    }

    fn collateral_safes(&self) -> Vec<Address> {
        let events = self.events.lock().expect("recorder poisoned");
        if events.first() != Some(&KIND_RELAY_CONFIG) || events.len() < 13 {
            return Vec::new();
        }
        let len = u32::from_le_bytes(events[9..13].try_into().expect("4 bytes")) as usize;
        let Ok(config) =
            <helix_tcp_types::merging::control::RelayConfigV1 as ssz::Decode>::from_ssz_bytes(
                &events[13..13 + len],
            )
        else {
            return Vec::new();
        };
        config
            .builder_collaterals
            .iter()
            .map(|collateral| Address::from_slice(collateral.collateral_safe.as_slice()))
            .collect()
    }
}

/// A key's proof plus, for each branch on its path with exactly two children,
/// the other child: deleting the key collapses the branch into that node.
fn proof_with_siblings(
    trie: &ethrex_trie::Trie,
    key: &[u8],
) -> Result<Vec<Vec<u8>>, ethrex_trie::TrieError> {
    use ethrex_rlp::decode::RLPDecode;
    use ethrex_trie::{Nibbles, Node};
    let proof = trie.get_proof(key)?;
    let path = Nibbles::from_bytes(key);
    let mut at = Nibbles::default();
    let mut siblings = Vec::new();
    for encoded in &proof {
        let Ok(node) = Node::decode(encoded) else { break };
        match node {
            Node::Branch(branch) => {
                let children: Vec<usize> =
                    (0..16).filter(|&i| branch.choices[i].is_valid()).collect();
                let next = path.at(at.len());
                if let [a, b] = children[..] {
                    let sibling = if a == next { b } else { a };
                    let mut nibbles = at.append_new(sibling as u8).as_ref().to_vec();
                    nibbles.resize(64, 0);
                    let through: Vec<u8> =
                        nibbles.chunks(2).map(|pair| (pair[0] << 4) | pair[1]).collect();
                    siblings.extend(trie.get_proof(&through)?);
                }
                at = at.append_new(next as u8);
            }
            Node::Extension(extension) => at = at.concat(&extension.prefix),
            Node::Leaf(_) => break,
        }
    }
    Ok(proof.into_iter().chain(siblings).collect())
}

fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    std::fs::write(path, serde_json::to_vec(value).map_err(std::io::Error::other)?)
}

struct RecordingDatabase {
    inner: Arc<dyn Database>,
    recorder: Arc<SlotRecorder>,
}

impl Database for RecordingDatabase {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        let state = self.inner.get_account_state(address)?;
        if state == AccountState::default() {
            self.recorder.absent.insert(address);
        } else {
            self.recorder.accounts.insert(address, state);
        }
        Ok(state)
    }

    fn get_storage_value(&self, address: Address, key: H256) -> Result<U256, DatabaseError> {
        let value = self.inner.get_storage_value(address, key)?;
        self.recorder.storage.insert((address, key), value);
        Ok(value)
    }

    fn get_block_hash(&self, block_number: u64) -> Result<H256, DatabaseError> {
        let hash = self.inner.get_block_hash(block_number)?;
        self.recorder.block_hashes.insert(block_number, hash);
        Ok(hash)
    }

    fn get_chain_config(&self) -> Result<ChainConfig, DatabaseError> {
        self.inner.get_chain_config()
    }

    fn get_account_code(&self, code_hash: H256) -> Result<Code, DatabaseError> {
        let code = self.inner.get_account_code(code_hash)?;
        self.recorder.codes.insert(code_hash, code.clone());
        Ok(code)
    }

    fn get_code_metadata(&self, code_hash: H256) -> Result<CodeMetadata, DatabaseError> {
        self.inner.get_code_metadata(code_hash)
    }
}
