use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::SystemTime,
};

use futures::StreamExt;
use reth_chain_state::{CanonStateNotification, CanonStateNotificationStream};

const RETAIN_BLOCKS: usize = 14400;

pub async fn run_block_state_recorder(
    mut notifications: CanonStateNotificationStream,
    record_dir: String,
) {
    // Read existing record dir contents.
    let mut files = Vec::new();
    if let Ok(mut all_files) = tokio::fs::read_dir(&record_dir).await {
        while let Ok(Some(entry)) = all_files.next_entry().await {
            if let Ok(metadata) = entry.metadata().await {
                let path = entry.path();
                files.push((path, metadata.created().unwrap_or_else(|_| SystemTime::now())));
            }
        }
    }
    files.sort_by_key(|(_, time)| *time);
    let mut files: VecDeque<(PathBuf, SystemTime)> = files.into();

    while let Some(notification) = notifications.next().await {
        if let CanonStateNotification::Commit { new } = notification {
            let block_number = new.first().number;
            let outcome = new.execution_outcome();

            match serde_json::to_string(&outcome.bundle) {
                Ok(json) => {
                    let path = format!("{record_dir}/{block_number}.json");
                    if let Err(e) = tokio::fs::write(&path, json).await {
                        tracing::error!(error=?e, block_number, "failed to write block updates");
                    }
                    let path_buf = Path::new(&path).to_path_buf();
                    files.push_back((path_buf, SystemTime::now()));

                    // Maybe cleanup old records.
                    while files.len() > RETAIN_BLOCKS {
                        match files.pop_front() {
                            Some((path, _)) => {
                                if let Err(e) = tokio::fs::remove_file(&path).await {
                                    tracing::error!(error=?e, ?path, "failed to delete state record");
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error=?e, block_number, "failed to serialise bundle state")
                }
            }
        }
    }
}
