use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use alloy_primitives::B256;
use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region, retry::RetryConfig},
    error::{DisplayErrorContext, ProvideErrorMetadata},
    primitives::ByteStream,
};
use helix_common::{S3Config, expect_env_var};
use uuid::Uuid;

use crate::auctioneer::InternalBidSubmissionHeader;

const ENV_ACCESS_KEY_ID: &str = "S3_ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "S3_SECRET_ACCESS_KEY";

pub struct S3Data {
    client: Client,
    bucket: String,
    failures: Arc<AtomicU32>,
}

impl S3Data {
    pub fn new(config: S3Config) -> Self {
        let access_key_id = expect_env_var(ENV_ACCESS_KEY_ID);
        let secret_access_key = expect_env_var(ENV_SECRET_ACCESS_KEY);

        let creds = Credentials::new(&access_key_id, &secret_access_key, None, None, "env");
        // A hand-built config retries nothing by default, so a `SlowDown` lost the object.
        // Adaptive mode also paces the client down while the bucket throttles us.
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(creds)
            .region(Region::new(config.region.clone()))
            .retry_config(RetryConfig::adaptive().with_max_attempts(3))
            .build();
        let client = Client::from_conf(sdk_config);

        Self { client, bucket: config.bucket, failures: Arc::default() }
    }

    /// Failures since the last call. Drained once per slot by the stats log.
    pub fn take_failures(&self) -> u32 {
        self.failures.swap(0, Ordering::Relaxed)
    }

    pub fn upload_task(
        &self,
        header: InternalBidSubmissionHeader,
        payload: &[u8],
        key_parts: Option<(u64, B256)>,
    ) -> impl Future<Output = ()> + Send + 'static {
        let id = header.id;
        let header = header.to_bytes();
        let header_slice = header.as_slice();
        let header_len = header_slice.len() as u16;

        // format: [u16 LE header_len][header bytes][payload bytes]
        let mut buf = bytes::BytesMut::with_capacity(2 + header_slice.len() + payload.len());
        buf.extend_from_slice(&header_len.to_le_bytes());
        buf.extend_from_slice(header_slice);
        buf.extend_from_slice(payload);
        let bytes = buf.freeze();

        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let failures = self.failures.clone();
        async move {
            let key = Self::make_key(id, key_parts);
            if let Err(e) = client
                .put_object()
                .bucket(bucket)
                .key(&key)
                .body(ByteStream::from(bytes))
                .send()
                .await &&
                failures.fetch_add(1, Ordering::Relaxed) == 0
            {
                let detail = e
                    .message()
                    .map(str::to_owned)
                    .unwrap_or_else(|| DisplayErrorContext(&e).to_string());
                tracing::error!(code = e.code().unwrap_or("none"), detail, %key, "s3 upload failed");
            }
        }
    }

    fn make_key(id: Uuid, key_parts: Option<(u64, B256)>) -> String {
        match key_parts {
            Some((slot, block_hash)) => format!("{slot}_{block_hash}.bin"),
            None => format!("unkeyed_{id}.bin"),
        }
    }
}
