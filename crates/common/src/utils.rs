#[cfg(test)]
pub mod test_utils {
    use ethereum_consensus::ssz::prelude::Serializable;
    use serde_json::Value;

    /// Test that the encoding and decoding works, returns the decoded struct
    pub fn test_encode_decode_json<T: serde::Serialize + serde::de::DeserializeOwned>(
        d: &str,
    ) -> T {
        let decoded = serde_json::from_str::<T>(d).expect("deserialize");

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

    pub fn test_encode_decode_ssz<T: Serializable>(d: &[u8]) -> T {
        let decoded = T::deserialize(d).expect("deserialize");
        let mut encoded = Vec::new();
        decoded.serialize(&mut encoded).unwrap();

        assert_eq!(encoded, d);

        decoded
    }
}
