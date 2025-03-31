use bitflags::bitflags;
use ethereum_consensus::{altair::BlsPublicKey, primitives::Hash32, ssz::prelude::*};
use std::{
    fmt::{Debug, Formatter},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};

use crate::{
    bellatrix::SimpleSerialize, bid_submission::v2::header_submission::SignedHeaderSubmission,
};

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
    pub struct MessageHeaderFlags: u32 {
        const SSZ_ENCODED = 1 << 0;
        const JSON_ENCODED = 1 << 1;
        const CBOR_ENCODED = 1 << 2;
        const CANCELLATION_ENABLED = 1 << 3;
    }
}

impl Debug for MessageHeaderFlags {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageHeaderFlags").field("value", &self.0 .0).finish()
    }
}

/// Message header for messages between builder and relay for V3 optimistic submission.
#[derive(Debug)]
#[repr(C)]
pub struct MessageHeader {
    /// The message type.
    pub message_type: MessageType,
    pub padding: u8,
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
    /// Flags describing the message.
    pub message_flags: MessageHeaderFlags,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct PayloadSocketAddressIpV4 {
    pub ip: u32,
    pub port: u16,
    pub sequence: u32,
    pub builder_pubkey: BlsPublicKey,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct PayloadSocketAddressIpV6 {
    pub ip: u128,
    pub port: u16,
    pub sequence: u32,
    pub builder_pubkey: BlsPublicKey,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Serializable, serde::Serialize, serde::Deserialize)]
pub struct HeaderSubmissionV3 {
    pub payload_socket_address: PayloadSocketAddress,
    pub submission: SignedHeaderSubmission,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GetPayloadV3 {
    pub block_hash: Hash32,
    pub request_ts: u64,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
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

impl From<&SocketAddr> for PayloadSocketAddress {
    fn from(value: &SocketAddr) -> Self {
        match value {
            SocketAddr::V4(v4) => PayloadSocketAddress::IpV4(PayloadSocketAddressIpV4 {
                ip: v4.ip().to_bits(),
                port: v4.port(),
                sequence: 0,
                builder_pubkey: BlsPublicKey::default(),
            }),
            SocketAddr::V6(v6) => PayloadSocketAddress::IpV6(PayloadSocketAddressIpV6 {
                ip: v6.ip().to_bits(),
                port: v6.port(),
                sequence: 0,
                builder_pubkey: BlsPublicKey::default(),
            }),
        }
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
