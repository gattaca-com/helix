use std::{
    fmt::{Debug, Formatter},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};

use alloy_primitives::B256;
use bitflags::bitflags;
use helix_types::{BlsPublicKey, BlsPublicKeyBytes, BlsSignatureBytes};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::bid_submission::v2::header_submission::SignedHeaderSubmission;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MessageType {
    HeaderSubmission = 0,
    HeaderSubmissionAck = 1,
    GetPayloadRequest = 2,
    GetPayloadResponse = 3,
    Error = 4,
}

bitflags! {
    #[derive(Clone, Copy)]
    pub struct MessageHeaderFlags: u16 {
        const SSZ_ENCODED = 1 << 0;
        const JSON_ENCODED = 1 << 1;
        const CBOR_ENCODED = 1 << 2;
        // const CANCELLATION_ENABLED = 1 << 3;
        const NO_SHARE = 1 <<4;
    }
}

impl Debug for MessageHeaderFlags {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageHeaderFlags").field("value", &self.0 .0).finish()
    }
}

/// Message header for messages between builder and relay for V3 optimistic submission.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MessageHeader {
    /// The message type.
    pub message_type: MessageType,
    pub padding: u8,
    /// Flags describing the message.
    pub message_flags: MessageHeaderFlags,
    /// The encoded length of the message bytes that follow the `MessageHeader`. The message bytes
    /// for different message types are:
    ///  - `HeaderSubmission`: encoded `SignedHeaderSubmission`
    ///  - `HeaderSubmissionAck`: nothing
    ///  - `GetPayloadRequest`: encoded `GetPayloadV3`
    ///  - `GetPayloadResponse`: encoded `PayloadAndBlobs`
    pub message_length: u32,
    /// Submission sequence number set by the builder. The sequence number from the original
    /// `HeaderSubmission` is returned on the `HeaderSubmissionAck` and the `GetPayloadRequest`
    pub sequence_number: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Encode, Decode)]
pub struct PayloadSocketAddressIpV4 {
    pub ip: u32,
    pub port: u16,
    pub sequence: u32,
    pub builder_pubkey: BlsPublicKey,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Encode, Decode)]
pub struct PayloadSocketAddressIpV6 {
    pub ip: u128,
    pub port: u16,
    pub sequence: u32,
    pub builder_pubkey: BlsPublicKey,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
pub enum PayloadSocketAddress {
    IpV4(PayloadSocketAddressIpV4),
    IpV6(PayloadSocketAddressIpV6),
}

impl PayloadSocketAddress {
    pub fn ip(&self) -> IpAddr {
        match self {
            PayloadSocketAddress::IpV4(v4) => IpAddr::V4(Ipv4Addr::from_bits(v4.ip)),
            PayloadSocketAddress::IpV6(v6) => IpAddr::V6(Ipv6Addr::from_bits(v6.ip)),
        }
    }

    pub fn seq(&self) -> u32 {
        match self {
            PayloadSocketAddress::IpV4(v4) => v4.sequence,
            PayloadSocketAddress::IpV6(v6) => v6.sequence,
        }
    }

    pub fn builder_pubkey(&self) -> &BlsPublicKey {
        match self {
            PayloadSocketAddress::IpV4(v4) => &v4.builder_pubkey,
            PayloadSocketAddress::IpV6(v6) => &v6.builder_pubkey,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Encode, Decode)]
pub struct HeaderSubmissionV3 {
    pub url: Vec<u8>,
    pub tx_count: u32,
    pub submission: SignedHeaderSubmission,
}

/// Request sent to builders for block payloads.
#[derive(Debug, serde::Deserialize, serde::Serialize, Encode, Decode, TreeHash)]
pub struct GetPayloadV3 {
    /// Hash of the block header from the `SignedHeaderSubmission`.
    pub block_hash: B256,
    /// Timestamp (in milliseconds) when the relay made this request.
    pub request_ts_millis: u64,
    /// Relay's public key
    pub relay_pubkey: BlsPublicKeyBytes,
}

impl helix_types::SignedRoot for GetPayloadV3 {}

/// Builder block payload response.
#[derive(Debug, serde::Deserialize, serde::Serialize, Encode, Decode)]
pub struct SignedGetPayloadV3 {
    pub message: GetPayloadV3,
    /// Signature from the relay's key that it uses to sign the `get_header`
    /// responses.
    pub signature: BlsSignatureBytes,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Encode, Decode)]
pub struct SubmissionV3Error {
    pub status: u16,
}

impl AsRef<[u8]> for MessageHeader {
    fn as_ref(&self) -> &[u8] {
        let ptr = std::ptr::addr_of!(*self);
        unsafe { std::slice::from_raw_parts(ptr as *const u8, size_of::<MessageHeader>()) }
    }
}

impl From<&[u8]> for &MessageHeader {
    fn from(value: &[u8]) -> Self {
        unsafe { &*(value.as_ptr() as *const MessageHeader) }
    }
}

impl From<&mut [u8]> for &mut MessageHeader {
    fn from(value: &mut [u8]) -> Self {
        unsafe { &mut *(value.as_ptr() as *mut MessageHeader) }
    }
}

impl From<&PayloadSocketAddress> for SocketAddr {
    fn from(value: &PayloadSocketAddress) -> Self {
        match value {
            PayloadSocketAddress::IpV4(v4) => {
                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from_bits(v4.ip), v4.port))
            }
            PayloadSocketAddress::IpV6(v6) => {
                SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from_bits(v6.ip), v6.port, 0, 0))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::bid_submission::{
        v2::header_submission::HeaderSubmissionElectra,
        v3::header_submission_v3::HeaderSubmissionV3,
    };

    const V3: &str = "{\"url\":[104,116,116,112,58,47,47,49,48,46,48,46,48,46,49,49,51,58,56,48,56,48],\
    \"submission\":{\"message\":{\"bid_trace\":{\"slot\":\"4119478\",\"parent_hash\":\"0x96b6f0a8ab15e18db22a6e467b0404ba667ebe864a7ebe294d27fee9ba626e5c\",\"block_hash\":\"0x2165fab735f3a81b357500da98073fa0a29b01e27e8e649937d14ac9970be651\",\"builder_pubkey\":\"0xabf8cd171567b94fe9b8ca6b516bfcffa9e5f2cbfe1e703c4429e1b0488e4e8011e3b77098dfad273fa8d1eb72cd7d31\",\"proposer_pubkey\":\"0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\",\"proposer_fee_recipient\":\"0x0000000000000000000000000000000000000000\",\"gas_limit\":\"36000000\",\"gas_used\":\"7449769\",\"value\":\"52145742593585955\"},\"execution_payload_header\":{\"parent_hash\":\"0x96b6f0a8ab15e18db22a6e467b0404ba667ebe864a7ebe294d27fee9ba626e5c\",\"fee_recipient\":\"0x148914866080716b10d686f5570631fbb2207002\",\"state_root\":\"0xdd1be8b46eaf6a7c5d8a5fd37d323fe76fe8bb838479df0ad796e70a1c3472e7\",\"receipts_root\":\"0x51e663b3bcfa35ab5e8d96747a798c9356f03decdad366f3ea54bdce8baa7a3c\",\"logs_bloom\":\"0x000801c300020104001400100000022600d144130000000014800848004000020600008020200008000004000000010800340024012402004000000404321110000001608088028200910c880612100098010240008800602001090020a208000c00054012000000000211800280880000000600000500180200009809484040020100c480002800a09820200020080000060031000080000094020494100020e20804810a8100008001010020040140042400200900008888200048000860021000000340402008042000110004009000000820800c001002900028200060044298901810040800100020000104000004400002002100940000400000010002\",\"prev_randao\":\"0x5a7b9c5133853b787094f0af36232ae03f2319d31621f289ae97b63a6a2089e3\",\"block_number\":\"3716606\",\"gas_limit\":\"36000000\",\"gas_used\":\"7449769\",\"timestamp\":\"1745336136\",\"extra_data\":\"0x546974616e2028746974616e6275696c6465722e78797a29\",\"base_fee_per_gas\":\"9\",\"block_hash\":\"0x2165fab735f3a81b357500da98073fa0a29b01e27e8e649937d14ac9970be651\",\"transactions_root\":\"0x4e97c8c414e7bf628c8ba484ace6c1b1d5c0406884bac538b148509a6fba9854\",\"withdrawals_root\":\"0xda3f20dc5aba6807e2d21b39da5a5d927be03f235709a909bef6fe77b0dc4250\",\"blob_gas_used\":\"131072\",\"excess_blob_gas\":\"0\"},\"execution_requests\":{\"deposits\":[],\"withdrawals\":[],\"consolidations\":[]},\"commitments\":[\"0xab7868350150390c8d1d8fbd447ad8ff743c883ff11d24cc32f28d8f8f5ccb2d277aa251615b04fff46fabe666783e5a\"]},\"signature\":\"0xaf32fae5ee787e565f26d0e267c24805ace458d4fd2fbfb48c985d7d74a33f13b2e7ead0d9d6c16a4471f951bde5966e0520f5c52d8594e6686e07ff93c62ad238fe9a9e7a084b80d21a857de4129a8ae3b478ea22a2b96ecbc72fa9466e66aa\"}}";
    const SUB: &str = "{\"bid_trace\":{\"slot\":\"4119478\",\"parent_hash\":\"0x96b6f0a8ab15e18db22a6e467b0404ba667ebe864a7ebe294d27fee9ba626e5c\",\"block_hash\":\"0x2165fab735f3a81b357500da98073fa0a29b01e27e8e649937d14ac9970be651\",\"builder_pubkey\":\"0xabf8cd171567b94fe9b8ca6b516bfcffa9e5f2cbfe1e703c4429e1b0488e4e8011e3b77098dfad273fa8d1eb72cd7d31\",\"proposer_pubkey\":\"0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\",\"proposer_fee_recipient\":\"0x0000000000000000000000000000000000000000\",\"gas_limit\":\"36000000\",\"gas_used\":\"7449769\",\"value\":\"52145742593585955\"},\"execution_payload_header\":{\"parent_hash\":\"0x96b6f0a8ab15e18db22a6e467b0404ba667ebe864a7ebe294d27fee9ba626e5c\",\"fee_recipient\":\"0x148914866080716b10d686f5570631fbb2207002\",\"state_root\":\"0xdd1be8b46eaf6a7c5d8a5fd37d323fe76fe8bb838479df0ad796e70a1c3472e7\",\"receipts_root\":\"0x51e663b3bcfa35ab5e8d96747a798c9356f03decdad366f3ea54bdce8baa7a3c\",\"logs_bloom\":\"0x000801c300020104001400100000022600d144130000000014800848004000020600008020200008000004000000010800340024012402004000000404321110000001608088028200910c880612100098010240008800602001090020a208000c00054012000000000211800280880000000600000500180200009809484040020100c480002800a09820200020080000060031000080000094020494100020e20804810a8100008001010020040140042400200900008888200048000860021000000340402008042000110004009000000820800c001002900028200060044298901810040800100020000104000004400002002100940000400000010002\",\"prev_randao\":\"0x5a7b9c5133853b787094f0af36232ae03f2319d31621f289ae97b63a6a2089e3\",\"block_number\":\"3716606\",\"gas_limit\":\"36000000\",\"gas_used\":\"7449769\",\"timestamp\":\"1745336136\",\"extra_data\":\"0x546974616e2028746974616e6275696c6465722e78797a29\",\"base_fee_per_gas\":\"9\",\"block_hash\":\"0x2165fab735f3a81b357500da98073fa0a29b01e27e8e649937d14ac9970be651\",\"transactions_root\":\"0x4e97c8c414e7bf628c8ba484ace6c1b1d5c0406884bac538b148509a6fba9854\",\"withdrawals_root\":\"0xda3f20dc5aba6807e2d21b39da5a5d927be03f235709a909bef6fe77b0dc4250\",\"blob_gas_used\":\"131072\",\"excess_blob_gas\":\"0\"},\"execution_requests\":{\"deposits\":[],\"withdrawals\":[],\"consolidations\":[]},\"commitments\":[\"0xab7868350150390c8d1d8fbd447ad8ff743c883ff11d24cc32f28d8f8f5ccb2d277aa251615b04fff46fabe666783e5a\"]}}";
    #[test]
    fn decode() {
        let v3: HeaderSubmissionV3 = serde_json::from_str(V3).unwrap();
        assert_eq!(4119478, v3.submission.bid_trace().slot);
    }

    #[test]
    fn decode_electra() {
        let sub: HeaderSubmissionElectra = serde_json::from_str(SUB).unwrap();
        assert_eq!(4119478, sub.bid_trace.slot);
    }
}
