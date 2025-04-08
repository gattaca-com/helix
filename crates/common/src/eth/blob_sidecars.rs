use std::sync::Arc;

use helix_types::{BlobSidecar, BlobSidecarError, BlobSidecars, SszError, VersionedSignedProposal};
use tracing::error;

#[derive(Debug, thiserror::Error)]
pub enum BuildBlobSidecarError {
    #[error("kzg proof mismatch: proofs: {proofs}, blobs: {blobs}")]
    KzgProofMismatch { proofs: usize, blobs: usize },
    #[error("blob sidecar error: {0:?}")]
    BlobSidecarError(BlobSidecarError),
    #[error("Ssz error: {0:?}")]
    SszError(SszError),
}

// TODO: avoid cloning blobs
pub fn blob_sidecars_from_unblinded_payload(
    unblinded_payload: &VersionedSignedProposal,
) -> Result<BlobSidecars, BuildBlobSidecarError> {
    if unblinded_payload.blobs.len() != unblinded_payload.kzg_proofs.len() {
        return Err(BuildBlobSidecarError::KzgProofMismatch {
            proofs: unblinded_payload.kzg_proofs.len(),
            blobs: unblinded_payload.blobs.len(),
        });
    }

    let mut blob_sidecars = Vec::with_capacity(unblinded_payload.blobs.len());

    for (index, blob) in unblinded_payload.blobs.iter().enumerate() {
        // this is safe cause we checked the length of the blobs and proofs
        let kzg_proof = unblinded_payload.kzg_proofs[index];
        let sidecar =
            BlobSidecar::new(index, blob.clone(), &unblinded_payload.signed_block, kzg_proof)
                .map_err(BuildBlobSidecarError::BlobSidecarError)?;
        blob_sidecars.push(Arc::new(sidecar));
    }

    // TODO: check max
    let sidecars =
        BlobSidecars::new(blob_sidecars, 4096).map_err(BuildBlobSidecarError::SszError)?;

    Ok(sidecars)
}

// #[cfg(test)]
// mod tests {

//     #[test]
//     // this is from mev-boost test data
//     fn test_signed_blinded_block_fb() {
//         let data = include_str!("testdata/signed-blinded-beacon-block-deneb.json");
//         let block = test_encode_decode_json::<SignedBlindedBeaconBlock>(&data);
//         assert!(matches!(block.message(), BlindedBeaconBlockRef::Deneb(_)));
//     }

//     #[test]
//     // this is from the builder api spec, but with sync_committee_bits fixed to
//     // deserialize correctly
//     fn test_signed_blinded_beacon_block() {
//         let data = include_str!("testdata/signed-blinded-beacon-block-deneb-2.json");
//         let block_json = test_encode_decode_json::<SignedBlindedBeaconBlock>(&data);
//         assert!(matches!(block_json.message(), BlindedBeaconBlockRef::Deneb(_)));
//     }

//     #[test]
//     // this is dummy data generated with https://github.com/attestantio/go-builder-client
//     fn test_execution_payload_block_ssz() {
//         let data_json = include_str!("testdata/execution-payload-deneb.json");
//         test_encode_decode_json::<PayloadAndBlobs>(&data_json);
//     }

// }

// #[cfg(test)]
// mod tests {
//     use ethereum_consensus::{
//         ssz::prelude::*,
//         types::{mainnet::SignedBlindedBeaconBlock, BlindedBeaconBlockRef},
//     };

//     use crate::{
//         bid_submission::SignedBidSubmissionElectra,
//         utils::test_utils::{test_encode_decode_json, test_encode_decode_ssz},
//         SignedBuilderBid,
//     };

//     #[test]
//     fn test_signed_builder_electra() {
//         let json = r#"{
//             "version": "electra",
//             "data": {
//                 "message": {
//                     "header": {
//                         "parent_hash":
// "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
// "fee_recipient": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
// "state_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
// "receipts_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
//                         "logs_bloom":
// "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
// ,                         "prev_randao":
// "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
// "block_number": "1",                         "gas_limit": "1",
//                         "gas_used": "1",
//                         "timestamp": "1",
//                         "extra_data":
// "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
// "base_fee_per_gas": "1",                         "blob_gas_used": "1",
//                         "excess_blob_gas": "1",
//                         "block_hash":
// "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
// "transactions_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
//                         "withdrawals_root":
// "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2"                     },
//                     "blob_kzg_commitments": [
//
// "0xa94170080872584e54a1cf092d845703b13907f2e6b3b1c0ad573b910530499e3bcd48c6378846b80d2bfa58c81cf3d5"
//                     ],
//                     "execution_requests": {
//                         "deposits": [
//                             {
//                                 "pubkey":
// "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
// ,                                 "withdrawal_credentials":
// "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
// "amount": "1",                                 "signature":
// "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
// ,                                 "index": "1"
//                             }
//                         ],
//                         "withdrawals": [
//                             {
//                                 "source_address": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
//                                 "validator_pubkey":
// "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
// ,                                 "amount": "1"
//                             }
//                         ],
//                         "consolidations": [
//                             {
//                                 "source_address": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
//                                 "source_pubkey":
// "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
// ,                                 "target_pubkey":
// "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
//                             }
//                         ]
//                     },
//                     "value": "1",
//                     "pubkey":
// "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
//                 },
//                 "signature":
// "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
//             }
//         }"#;

//         let signed_builder_bid =
//             crate::utils::test_utils::test_encode_decode_json::<SignedBuilderBid>(json);

//         assert_eq!(signed_builder_bid.version(), "electra");
//         assert!(matches!(signed_builder_bid, SignedBuilderBid::Electra(_, None)));
//     }

//     #[test]
//     // this is from mev-boost test data
//     fn test_signed_builder_bid_2() {
//         let data_json = include_str!("testdata/signed-builder-bid-electra.json");
//         test_encode_decode_json::<super::SignedBuilderBid>(&data_json);
//     }

//     #[test]
//     // this is from mev-boost test data
//     fn test_signed_blinded_block_fb() {
//         let data_json = include_str!("testdata/signed-blinded-beacon-block-electra.json");
//         let block_json = test_encode_decode_json::<SignedBlindedBeaconBlock>(&data_json);
//         assert!(matches!(block_json.message(), BlindedBeaconBlockRef::Electra(_)));
//     }

//     #[test]
//     // this is dummy data generated with https://github.com/attestantio/go-eth2-client
//     fn test_signed_blinded_beacon_block() {
//         let data_json = include_str!("testdata/signed-blinded-beacon-block-electra-2.json");
//         let block_json = test_encode_decode_json::<SignedBlindedBeaconBlock>(&data_json);
//         assert!(matches!(block_json.message(), BlindedBeaconBlockRef::Electra(_)));

//         let mut encoded = Vec::new();
//         let written = block_json.serialize(&mut encoded).unwrap();
//         assert!(written > 0);

//         let data_ssz = include_bytes!("testdata/signed-blinded-beacon-block-electra-2.ssz");
//         let data_ssz = alloy_primitives::hex::decode(data_ssz).unwrap();

//         assert_eq!(encoded, data_ssz);
//         let block_ssz = test_encode_decode_ssz::<SignedBlindedBeaconBlock>(&data_ssz);
//         assert!(matches!(block_ssz.message(), BlindedBeaconBlockRef::Electra(_)));

//         let mut encoded = Vec::new();
//         let written = block_json.serialize(&mut encoded).unwrap();
//         assert!(written > 0);

//         assert_eq!(encoded, data_ssz);
//     }

// }
