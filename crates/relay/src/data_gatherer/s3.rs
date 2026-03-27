use std::future::Future;

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    primitives::ByteStream,
};
use chrono::Utc;
use helix_common::{S3Config, expect_env_var};
use uuid::Uuid;

use crate::auctioneer::InternalBidSubmissionHeader;

const ENV_ACCESS_KEY_ID: &str = "S3_ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "S3_SECRET_ACCESS_KEY";

pub struct S3Data {
    client: Client,
    bucket: String,
}

impl S3Data {
    pub fn new(config: S3Config) -> Self {
        let access_key_id = expect_env_var(ENV_ACCESS_KEY_ID);
        let secret_access_key = expect_env_var(ENV_SECRET_ACCESS_KEY);

        let creds = Credentials::new(&access_key_id, &secret_access_key, None, None, "env");
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(creds)
            .region(Region::new(config.region.clone()))
            .build();
        let client = Client::from_conf(sdk_config);

        Self { client, bucket: config.bucket }
    }

    pub fn upload_task(
        &self,
        header: InternalBidSubmissionHeader,
        payload: &[u8],
        payload_offset: usize,
    ) -> impl Future<Output = ()> + Send + 'static {
        let id = header.id;
        let header = header.to_bytes();
        let header_slice = header.as_slice();
        let header_len = header_slice.len() as u16;

        let payload = &payload[payload_offset..];
        // format: [u16 LE header_len][header bytes][payload bytes]
        let mut buf = bytes::BytesMut::with_capacity(2 + header_slice.len() + payload.len());
        buf.extend_from_slice(&header_len.to_le_bytes());
        buf.extend_from_slice(header_slice);
        buf.extend_from_slice(payload);
        let bytes = buf.freeze();

        let client = self.client.clone();
        let bucket = self.bucket.clone();
        async move {
            let key = Self::make_key(id);
            if let Err(e) = client
                .put_object()
                .bucket(bucket)
                .key(&key)
                .body(ByteStream::from(bytes))
                .send()
                .await
            {
                tracing::error!(%e, %key, "s3 upload failed");
            }
        }
    }

    fn make_key(id: Uuid) -> String {
        let now = Utc::now().to_rfc3339();
        format!("{now}_{id}.bin")
    }
}
