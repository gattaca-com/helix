use std::sync::Arc;

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    primitives::ByteStream,
};
use bytes::Bytes;
use chrono::Utc;
use flux::tile::Tile;
use helix_common::S3Config;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{HelixSpine, spine::messages::NewBidSubmission};

const ENV_ACCESS_KEY_ID: &str = "S3_ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "S3_SECRET_ACCESS_KEY";

pub struct S3PayloadSaver {
    tx: mpsc::Sender<(NewBidSubmission, Bytes)>,
}

impl S3PayloadSaver {
    pub fn new(config: S3Config) -> Self {
        let access_key_id = std::env::var(ENV_ACCESS_KEY_ID)
            .unwrap_or_else(|_| panic!("{ENV_ACCESS_KEY_ID} must be set"));
        let secret_access_key = std::env::var(ENV_SECRET_ACCESS_KEY)
            .unwrap_or_else(|_| panic!("{ENV_SECRET_ACCESS_KEY} must be set"));

        let creds = Credentials::new(&access_key_id, &secret_access_key, None, None, "env");
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(creds)
            .region(Region::new(config.region.clone()))
            .build();
        let client = Client::from_conf(sdk_config);
        let bucket = Arc::<str>::from(config.bucket.as_str());

        let (tx, mut rx) = mpsc::channel::<(NewBidSubmission, Bytes)>(10_000);
        tokio::spawn(async move {
            while let Some((r, payload)) = rx.recv().await {
                let key = make_key(r.header.id);
                let client = client.clone();
                let bucket = Arc::clone(&bucket);
                tokio::spawn(async move {
                    if let Err(e) = client
                        .put_object()
                        .bucket(bucket.as_ref())
                        .key(&key)
                        .body(ByteStream::from(payload))
                        .send()
                        .await
                    {
                        tracing::error!(%e, %key, "s3 upload failed");
                    }
                });
            }
        });

        Self { tx }
    }
}

impl Tile<HelixSpine> for S3PayloadSaver {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume_with_dcache(
            |r: NewBidSubmission, full_payload| {
                let header = r.header.to_bytes();
                let header_slice = header.as_slice();
                let header_len = header_slice.len() as u16;
                let payload_offset = r.payload_offset;

                let payload = &full_payload[payload_offset..];

                // format: [u16 LE header_len][header bytes][payload bytes]
                let mut buf =
                    bytes::BytesMut::with_capacity(2 + header_slice.len() + payload.len());
                buf.extend_from_slice(&header_len.to_le_bytes());
                buf.extend_from_slice(header_slice);
                buf.extend_from_slice(payload);
                let bytes = buf.freeze();

                if self.tx.try_send((r, bytes)).is_err() {
                    tracing::error!("s3 channel full, dropping payload");
                }
            },
            |_, _| {},
        );
    }
}

fn make_key(id: Uuid) -> String {
    let now = Utc::now().to_rfc3339();
    format!("{now}_{id}.bin")
}
