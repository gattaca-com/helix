use helix_tcp_types::merging::{MergingFrameHeader, MergingMsgId};
use ssz::Encode;

/// Bodies above this size are zstd-compressed when the peer negotiated it.
pub const ZSTD_THRESHOLD_BYTES: usize = 4096;
const ZSTD_LEVEL: i32 = 1;

/// Appends `[1B msg_type][1B flags][SSZ body]`; the outer
/// `[u32 len][u64 send-ts]` frame is written by the flux stream.
pub fn append_frame<T: Encode>(buf: &mut Vec<u8>, msg_id: MergingMsgId, msg: &T) {
    MergingFrameHeader::new(msg_id).append_encoded(buf);
    msg.ssz_append(buf);
}

/// Like [`append_frame`], but zstd-compresses large bodies when negotiated.
/// `scratch` is reused across calls to avoid allocation.
pub fn append_frame_maybe_zstd<T: Encode>(
    buf: &mut Vec<u8>,
    msg_id: MergingMsgId,
    msg: &T,
    zstd_negotiated: bool,
    scratch: &mut Vec<u8>,
) {
    if !zstd_negotiated {
        return append_frame(buf, msg_id, msg);
    }
    scratch.clear();
    msg.ssz_append(scratch);
    if scratch.len() <= ZSTD_THRESHOLD_BYTES {
        MergingFrameHeader::new(msg_id).append_encoded(buf);
        buf.extend_from_slice(scratch);
        return;
    }
    match zstd::bulk::compress(scratch, ZSTD_LEVEL) {
        Ok(compressed) => {
            MergingFrameHeader::new(msg_id).with_zstd().append_encoded(buf);
            buf.extend_from_slice(&compressed);
        }
        Err(err) => {
            tracing::warn!(%err, "zstd compression failed, sending uncompressed");
            MergingFrameHeader::new(msg_id).append_encoded(buf);
            buf.extend_from_slice(scratch);
        }
    }
}

/// Decompresses a zstd body, bounding the output at `max_bytes`.
pub fn decompress(body: &[u8], max_bytes: usize) -> Result<Vec<u8>, std::io::Error> {
    zstd::bulk::decompress(body, max_bytes)
}

#[cfg(test)]
mod tests {
    use helix_tcp_types::merging::{MERGING_HEADER_SIZE, control::PingV1};
    use ssz::Decode;

    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        append_frame(&mut buf, MergingMsgId::PingV1, &PingV1 { nonce: 42 });
        let header = MergingFrameHeader::decode(&buf).unwrap();
        assert_eq!(header.msg_id, MergingMsgId::PingV1);
        assert!(!header.is_zstd_compressed());
        assert_eq!(PingV1::from_ssz_bytes(&buf[MERGING_HEADER_SIZE..]).unwrap().nonce, 42);
    }

    #[test]
    fn small_bodies_stay_uncompressed_when_zstd_negotiated() {
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        append_frame_maybe_zstd(
            &mut buf,
            MergingMsgId::PongV1,
            &PingV1 { nonce: 1 },
            true,
            &mut scratch,
        );
        assert!(!MergingFrameHeader::decode(&buf).unwrap().is_zstd_compressed());
    }

    #[test]
    fn large_bodies_compress_and_roundtrip() {
        // RejectV1 with a msg > threshold
        use helix_tcp_types::merging::builder_to_relay::{RejectCode, RejectSubject, RejectV1};
        let msg = RejectV1 {
            slot: 1,
            code: RejectCode::Busy,
            subject: RejectSubject::None(0),
            msg: vec![7u8; 2 * ZSTD_THRESHOLD_BYTES],
        };
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        append_frame_maybe_zstd(&mut buf, MergingMsgId::RejectV1, &msg, true, &mut scratch);
        let header = MergingFrameHeader::decode(&buf).unwrap();
        assert!(header.is_zstd_compressed());
        let body = decompress(&buf[MERGING_HEADER_SIZE..], 10 * ZSTD_THRESHOLD_BYTES).unwrap();
        assert_eq!(RejectV1::from_ssz_bytes(&body).unwrap(), msg);
    }
}
