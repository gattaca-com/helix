use std::sync::Arc;

use alloy_consensus::Blob;
use alloy_primitives::{b256, B256};
use lh_types::{
    test_utils::{TestRandom, XorShiftRng},
    BeaconBlockElectra, BlindedPayload, FullPayload, MainnetEthSpec,
};
use rand::SeedableRng;
use serde_json::Value;
use ssz::{Decode, Encode};

use crate::{
    blobs::KzgCommitment, BlobsBundle, BlsPublicKey, BlsSecretKey, ExecutionPayloadElectra,
};

/// Test that the encoding and decoding works, returns the decoded struct
pub fn test_encode_decode_json<T: serde::Serialize + serde::de::DeserializeOwned>(d: &str) -> T {
    let decoded = serde_json::from_str::<T>(d).expect("deserialize json");

    // re-encode to make sure that different formats are ignored
    let encoded = serde_json::to_string(&decoded).unwrap();
    let original_v: Value = serde_json::from_str(d).unwrap();
    let encoded_v: Value = serde_json::from_str(&encoded).unwrap();

    if original_v != encoded_v {
        println!("ORIGINAL: {original_v}");
        println!("ENCODED: {encoded_v}");
        panic!("encode mismatch");
    }

    decoded
}

pub fn test_encode_decode_ssz<T: Encode + Decode>(d: &[u8]) -> T {
    let decoded = T::from_ssz_bytes(d).expect("deserialize ssz");
    let encoded = T::as_ssz_bytes(&decoded);

    assert_eq!(encoded, d);

    decoded
}

pub fn random_bls_pubkey() -> BlsPublicKey {
    BlsPublicKey::test_random()
}

pub fn get_fixed_pubkey(i: usize) -> BlsPublicKey {
    let key = get_fixed_secret(i);
    key.public_key()
}

pub fn get_fixed_secret(i: usize) -> BlsSecretKey {
    const KEYS: [B256; 5] = [
        b256!("64496d4e301e541a6e1237d6ef13a8f8b8b6cb82be9d8ac90073a833dfc2af11"),
        b256!("34c83cb0949c5f8d6e3142392b3b268de111b82004e48cc8f3049a4546753f81"),
        b256!("072bc82637a8213c59ea48adc9b15be242c540cd83c42b3b96ba63f66c924f5a"),
        b256!("12857a4a63eac6c8b2d9d690a0a31f2f0df2ae4988cd1dfa35f1a09075c7fae4"),
        b256!("0433221d4c34d24a71e8a137c6a96b4b81b92bc6c6b56bfda1dc9d4057466506"),
    ];

    let key = KEYS[i];
    let key = BlsSecretKey::deserialize(key.as_slice()).unwrap();
    key
}

pub fn get_payload_electra() -> (
    ExecutionPayloadElectra,
    BeaconBlockElectra<MainnetEthSpec, BlindedPayload<MainnetEthSpec>>,
    BlobsBundle,
) {
    let mut full_payload: BeaconBlockElectra<MainnetEthSpec, FullPayload<MainnetEthSpec>> =
        BeaconBlockElectra::test_random();

    full_payload.body.blob_kzg_commitments = Default::default();

    let execution_payload = full_payload.clone().body.execution_payload.execution_payload;
    let blinded = full_payload.clone_as_blinded();

    let mut blobs_bundle = BlobsBundle::test_random();
    blobs_bundle.commitments =
        blinded.body.blob_kzg_commitments.iter().map(|p| KzgCommitment::from(p.0)).collect();
    blobs_bundle.blobs =
        blobs_bundle.commitments.iter().map(|_| Arc::new(Blob::random())).collect::<Vec<_>>();

    (execution_payload, blinded, blobs_bundle)
}

pub fn initialize_test_tracing() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();
}

pub trait TestRandomSeed: TestRandom {
    fn test_random() -> Self
    where
        Self: Sized,
    {
        let mut rng = XorShiftRng::from_seed([42; 16]);
        Self::random_for_test(&mut rng)
    }
}

impl<T: TestRandom> TestRandomSeed for T {}
