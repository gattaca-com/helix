use std::{
    fs,
    io::{self, BufReader, BufWriter},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use helix_common::SignedValidatorRegistrationEntry;
use helix_types::BlsPublicKeyBytes;
use rustc_hash::FxHashSet;
use tracing::{info, warn};

const KNOWN_VALIDATORS_FILE: &str = "known_validators.bin";
const VALIDATOR_REGISTRATIONS_FILE: &str = "validator_registrations.json";

/// Atomically write `data` to `path` via a temp file + rename.
fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("bin.tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

// -- Known validators --

pub fn save_known_validators(dir: &Path, set: &FxHashSet<BlsPublicKeyBytes>) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let keys: Vec<&BlsPublicKeyBytes> = set.iter().collect();
    let data = bincode::serialize(&keys).map_err(io::Error::other)?;
    atomic_write(&dir.join(KNOWN_VALIDATORS_FILE), &data)?;
    info!(count = set.len(), "saved known_validators snapshot");
    Ok(())
}

pub fn load_known_validators(dir: &Path) -> io::Result<FxHashSet<BlsPublicKeyBytes>> {
    let path = dir.join(KNOWN_VALIDATORS_FILE);
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);
    let keys: Vec<BlsPublicKeyBytes> = bincode::deserialize_from(reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut set = FxHashSet::with_capacity_and_hasher(keys.len(), Default::default());
    for k in keys {
        set.insert(k);
    }
    info!(count = set.len(), "loaded known_validators from snapshot");
    Ok(set)
}

// -- Validator registrations --
//
// Format: (fetched_at unix ms, entries). `fetched_at` is captured before the DB fetch that
// produced the entries, so resuming incremental updates from it cannot miss registrations.

pub fn save_validator_registrations(
    dir: &Path,
    fetched_at: SystemTime,
    entries: &[(BlsPublicKeyBytes, SignedValidatorRegistrationEntry)],
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(VALIDATOR_REGISTRATIONS_FILE);
    let tmp = path.with_extension("bin.tmp");
    let file = fs::File::create(&tmp)?;
    let writer = BufWriter::new(file);
    let fetched_at_ms =
        fetched_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    serde_json::to_writer(writer, &(fetched_at_ms, entries)).map_err(io::Error::other)?;
    fs::rename(&tmp, &path)?;
    info!(count = entries.len(), "saved validator_registrations snapshot");
    Ok(())
}

pub fn load_validator_registrations(
    dir: &Path,
) -> io::Result<(SystemTime, Vec<(BlsPublicKeyBytes, SignedValidatorRegistrationEntry)>)> {
    let path = dir.join(VALIDATOR_REGISTRATIONS_FILE);
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);
    let (fetched_at_ms, entries): (
        u64,
        Vec<(BlsPublicKeyBytes, SignedValidatorRegistrationEntry)>,
    ) = serde_json::from_reader(reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    info!(count = entries.len(), "loaded validator_registrations from snapshot");
    Ok((UNIX_EPOCH + Duration::from_millis(fetched_at_ms), entries))
}

/// Try to load from snapshot, returning None on any failure (missing file, corrupt, etc).
pub async fn try_load_known_validators(dir: &Path) -> Option<FxHashSet<BlsPublicKeyBytes>> {
    let dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || load_known_validators(&dir)).await {
        Ok(Ok(set)) => Some(set),
        Ok(Err(e)) => {
            warn!("known_validators snapshot unavailable: {e}");
            None
        }
        Err(e) => {
            warn!("known_validators snapshot task failed: {e}");
            None
        }
    }
}

pub async fn try_load_validator_registrations(
    dir: &Path,
) -> Option<(SystemTime, Vec<(BlsPublicKeyBytes, SignedValidatorRegistrationEntry)>)> {
    let dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || load_validator_registrations(&dir)).await {
        Ok(Ok(snapshot)) => Some(snapshot),
        Ok(Err(e)) => {
            warn!("validator_registrations snapshot unavailable: {e}");
            None
        }
        Err(e) => {
            warn!("validator_registrations snapshot task failed: {e}");
            None
        }
    }
}

pub fn save_known_validators_bg(dir: &Path, set: &FxHashSet<BlsPublicKeyBytes>) {
    let dir = dir.to_path_buf();
    let keys: Vec<BlsPublicKeyBytes> = set.iter().copied().collect();
    tokio::task::spawn_blocking(move || {
        let mut s = FxHashSet::with_capacity_and_hasher(keys.len(), Default::default());
        for k in keys {
            s.insert(k);
        }
        if let Err(e) = save_known_validators(&dir, &s) {
            warn!("failed to save known_validators snapshot: {e}");
        }
    });
}
