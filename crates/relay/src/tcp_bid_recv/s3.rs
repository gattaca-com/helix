use aws_sdk_s3::{
    Client,
    config::{Credentials, Region},
    primitives::ByteStream,
};
use bytes::Bytes;
use chrono::Utc;
use helix_common::S3Config;
use tokio::sync::mpsc;
use uuid::Uuid;

const ENV_ACCESS_KEY_ID: &str = "S3_ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "S3_SECRET_ACCESS_KEY";

pub struct S3PayloadSaver {
    config: S3Config,
    access_key_id: String,
    secret_access_key: String,
}

impl S3PayloadSaver {
    pub fn new(config: S3Config) -> Self {
        let access_key_id = std::env::var(ENV_ACCESS_KEY_ID)
            .unwrap_or_else(|_| panic!("{ENV_ACCESS_KEY_ID} must be set"));
        let secret_access_key = std::env::var(ENV_SECRET_ACCESS_KEY)
            .unwrap_or_else(|_| panic!("{ENV_SECRET_ACCESS_KEY} must be set"));
        Self { config, access_key_id, secret_access_key }
    }

    pub fn spawn(self) -> mpsc::Sender<(Uuid, Bytes)> {
        let (tx, mut rx) = mpsc::channel(50_000);

        let creds =
            Credentials::new(&self.access_key_id, &self.secret_access_key, None, None, "env");
        let sdk_config = aws_sdk_s3::Config::builder()
            .credentials_provider(creds)
            .region(Region::new(self.config.region.clone()))
            .build();
        let client = Client::from_conf(sdk_config);

        tokio::spawn(async move {
            while let Some((request_id, payload)) = rx.recv().await {
                let key = make_key(request_id);
                if let Err(e) = client
                    .put_object()
                    .bucket(&self.config.bucket)
                    .key(&key)
                    .body(ByteStream::from(payload))
                    .send()
                    .await
                {
                    tracing::error!(%e, %key, "s3 upload failed");
                }
            }
        });

        tx
    }
}

fn make_key(request_id: Uuid) -> String {
    let now = Utc::now().to_rfc3339();
    format!("{now}_{request_id}.bin")
}
