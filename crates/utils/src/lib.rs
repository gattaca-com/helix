use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    panic,
    path::Path,
};

use ::serde::de;
use ethereum_consensus::{
    altair::{Bytes32, Slot},
    capella::Withdrawal,
    phase0::mainnet::SLOTS_PER_EPOCH,
    ssz::{self, prelude::SimpleSerialize},
};
use reth_primitives::{proofs, Address};
use tracing::{error, info};

pub mod request_encoding;
pub mod serde;
pub mod signing;

pub fn has_reached_fork(slot: u64, fork_epoch: u64) -> bool {
    if fork_epoch == 0 {
        return false
    }

    let current_epoch = slot / SLOTS_PER_EPOCH;
    current_epoch >= fork_epoch
}

// TODO: really need to fix the common here. Should probably just use reth common
pub fn calculate_withdrawals_root(withdrawals: &[Withdrawal]) -> [u8; 32] {
    let reth_withdrawals: Vec<reth_primitives::Withdrawal> =
        withdrawals.iter().cloned().map(to_reth_withdrawal).collect();
    proofs::calculate_withdrawals_root(&reth_withdrawals).0
}

fn to_reth_withdrawal(withdrawal: Withdrawal) -> reth_primitives::Withdrawal {
    reth_primitives::Withdrawal {
        index: withdrawal.index as u64,
        validator_index: withdrawal.validator_index as u64,
        address: Address::from_slice(withdrawal.address.as_ref()),
        amount: withdrawal.amount,
    }
}

pub fn try_decode_into<T>(is_ssz: bool, body_bytes: &[u8], json_fallback: bool) -> Option<T>
where
    T: SimpleSerialize + de::DeserializeOwned,
{
    if is_ssz {
        match ssz::prelude::deserialize::<T>(body_bytes).ok() {
            Some(decoded) => Some(decoded),
            None => {
                if json_fallback {
                    serde_json::from_slice(body_bytes).ok()
                } else {
                    None
                }
            }
        }
    } else {
        serde_json::from_slice(body_bytes).ok()
    }
}

pub fn get_payload_attributes_key(parent_hash: &Bytes32, slot: Slot) -> String {
    format!("{parent_hash:?}:{slot}")
}

pub fn set_panic_hook(
    instance_id: String,
    discord_web_hook: Option<String>,
    crash_log_path: Option<String>,
) {
    info!("setting panic hook...");
    panic::set_hook(Box::new(move |info| {
        let backtrace = backtrace::Backtrace::new();
        let crash_log = format!(
            "Panic: {info}\nFull backtrace:\n{backtrace:?}\n",
            info = info,
            backtrace = backtrace
        );
        println!("{}", crash_log);
        if let Some(crash_log_path) = crash_log_path.clone() {
            save_to_file(crash_log_path, crash_log.clone());
        }
        if let Some(discord_web_hook) = discord_web_hook.clone() {
            alert_discord(
                discord_web_hook,
                &format!("Relay: {} crashed! Please see the console log for details!", instance_id),
                &instance_id,
            );
        }
    }));
}

pub fn alert_discord(webhook_url: String, message: &str, region: &str) {
    let max_length = message.len().min(1850);
    let content = format!("Instance: RELAY-{}\n{}", region, &message[..max_length]);

    let mut payload = HashMap::new();
    payload.insert("content", content);

    let client = reqwest::blocking::Client::new();
    let result = client.post(webhook_url).json(&payload).send();

    if let Err(err) = result {
        error!(error=?err, "could not send alert to Discord\nMessage: {message}");
    }
}

pub fn save_to_file(path: String, json: String) {
    // Create the directory if it doesn't exist
    if let Some(parent_dir) = Path::new(&path).parent() {
        fs::create_dir_all(parent_dir).expect("Failed to create directory");
    }

    // Open the file, truncating it if it already exists
    let mut file = File::create(&path).expect("Failed to create file");

    // Write the JSON string to the file
    file.write_all(json.as_bytes()).expect("Failed to write JSON to file");
}
