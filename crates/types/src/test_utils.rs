use lh_types::test_utils::{TestRandom, XorShiftRng};
use rand::SeedableRng;
use serde_json::Value;
use ssz::{Decode, Encode};

use crate::BlsPublicKey;

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

pub fn get_fixed_pubkey() -> BlsPublicKey {
    BlsPublicKey::deserialize(&[0; 48]).unwrap()
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
