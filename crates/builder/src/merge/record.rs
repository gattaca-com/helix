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

use crate::spine::EventKind;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordConfig {
    pub dir: PathBuf,
    pub every_n_slots: u64,
}

// Fixture key, so no production key leaves the builder.
pub fn replay_signer() -> alloy_signer_local::PrivateKeySigner {
    "0x941e103320615d394a55708be13e45994c7d93b932b064dbcb2b511fe3254e2e"
        .parse()
        .expect("fixture key")
}

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

pub struct SlotRecorder {
    dir: PathBuf,
    slot: u64,
    relay_signer: Address,
    safes: Vec<Address>,
    events: Mutex<Vec<u8>>,
    accounts: DashMap<Address, AccountState>,
    absent: DashSet<Address>,
    storage: DashMap<(Address, H256), U256>,
    codes: DashMap<H256, Code>,
    block_hashes: DashMap<u64, H256>,
    pub parent_header: OnceLock<BlockHeader>,
    chain_config: OnceLock<ChainConfig>,
    parent_db: OnceLock<Arc<dyn Database>>,
    pub store: OnceLock<ethrex_storage::Store>,
}

impl SlotRecorder {
    pub fn new(
        config: &RecordConfig,
        slot: u64,
        relay_signer: Address,
        safes: Vec<Address>,
    ) -> Arc<Self> {
        Arc::new(Self {
            dir: config.dir.join(slot.to_string()),
            slot,
            relay_signer,
            safes,
            events: Mutex::default(),
            accounts: DashMap::default(),
            absent: DashSet::default(),
            storage: DashMap::default(),
            codes: DashMap::default(),
            block_hashes: DashMap::default(),
            parent_header: OnceLock::new(),
            chain_config: OnceLock::new(),
            parent_db: OnceLock::new(),
            store: OnceLock::new(),
        })
    }

    pub fn record_event(&self, recv_ns: u64, kind: EventKind, body: &[u8]) {
        if kind == EventKind::ConnectionReset {
            return;
        }
        let mut events = self.events.lock().expect("recorder poisoned");
        events.push(kind as u8);
        events.extend_from_slice(&recv_ns.to_le_bytes());
        events.extend_from_slice(&(body.len() as u32).to_le_bytes());
        events.extend_from_slice(body);
    }

    pub fn wrap_db(self: &Arc<Self>, parent_db: Arc<dyn Database>) -> Arc<dyn Database> {
        let _unused = self.parent_db.set(parent_db.clone());
        if let Ok(config) = parent_db.get_chain_config() {
            let _unused = self.chain_config.set(config);
        }
        Arc::new(RecordingDatabase { parent_db, recorder: self.clone() })
    }

    pub fn finish(self: Arc<Self>) {
        std::thread::spawn(move || {
            if let Err(err) = self.write_files() {
                tracing::warn!(slot = self.slot, %err, "slot recording failed");
            }
        });
    }

    fn write_files(&self) -> std::io::Result<()> {
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
                    .parent_db
                    .get()
                    .and_then(|parent_db| parent_db.get_account_code(entry.code_hash).ok())
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
            let proofs =
                self.trie_proofs(store, header.state_root).map_err(std::io::Error::other)?;
            let mut out =
                zstd::Encoder::new(std::fs::File::create(self.dir.join("trie.json.zst"))?, 3)?;
            out.write_all(&serde_json::to_vec(&proofs).map_err(std::io::Error::other)?)?;
            out.finish()?;
        }
        Ok(())
    }
}

impl SlotRecorder {
    fn trie_proofs(
        &self,
        store: &ethrex_storage::Store,
        state_root: H256,
    ) -> Result<TrieProofs, ethrex_storage::error::StoreError> {
        let replay_owner = Address::from_slice(replay_signer().address().as_slice());
        let mut slots: std::collections::BTreeMap<Address, Vec<H256>> = Default::default();
        for entry in self.storage.iter() {
            slots.entry(entry.key().0).or_default().push(entry.key().1);
        }
        for &safe in &self.safes {
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
}

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
    parent_db: Arc<dyn Database>,
    recorder: Arc<SlotRecorder>,
}

impl Database for RecordingDatabase {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        let state = self.parent_db.get_account_state(address)?;
        if state == AccountState::default() {
            self.recorder.absent.insert(address);
        } else {
            self.recorder.accounts.insert(address, state);
        }
        Ok(state)
    }

    fn get_storage_value(&self, address: Address, key: H256) -> Result<U256, DatabaseError> {
        let value = self.parent_db.get_storage_value(address, key)?;
        self.recorder.storage.insert((address, key), value);
        Ok(value)
    }

    fn get_block_hash(&self, block_number: u64) -> Result<H256, DatabaseError> {
        let hash = self.parent_db.get_block_hash(block_number)?;
        self.recorder.block_hashes.insert(block_number, hash);
        Ok(hash)
    }

    fn get_chain_config(&self) -> Result<ChainConfig, DatabaseError> {
        self.parent_db.get_chain_config()
    }

    fn get_account_code(&self, code_hash: H256) -> Result<Code, DatabaseError> {
        let code = self.parent_db.get_account_code(code_hash)?;
        self.recorder.codes.insert(code_hash, code.clone());
        Ok(code)
    }

    fn get_code_metadata(&self, code_hash: H256) -> Result<CodeMetadata, DatabaseError> {
        self.parent_db.get_code_metadata(code_hash)
    }
}
