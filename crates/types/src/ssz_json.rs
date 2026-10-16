use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum JsonSszError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{field}: {reason}")]
    Field { field: &'static str, reason: &'static str },
}

pub enum Ty {
    U64,
    Bool,
    Bytes(usize),
    ByteList,
    Vector(&'static Ty, usize),
    List(&'static Ty),
    Container(&'static [(&'static str, Ty)]),
}

impl Ty {
    fn fixed_size(&self) -> Option<usize> {
        match self {
            Ty::U64 => Some(8),
            Ty::Bool => Some(1),
            Ty::Bytes(n) => Some(*n),
            Ty::ByteList | Ty::List(_) => None,
            Ty::Vector(elem, n) => elem.fixed_size().map(|size| size * n),
            Ty::Container(fields) => fields.iter().map(|(_, ty)| ty.fixed_size()).sum(),
        }
    }

    pub fn encode(
        &self,
        field: &'static str,
        value: &Value,
        out: &mut Vec<u8>,
    ) -> Result<(), JsonSszError> {
        let err = |reason| JsonSszError::Field { field, reason };
        match self {
            Ty::U64 => {
                let v = match value {
                    Value::String(s) => s.parse().map_err(|_| err("not a uint64"))?,
                    Value::Number(n) => n.as_u64().ok_or(err("not a uint64"))?,
                    _ => return Err(err("not a uint64")),
                };
                out.extend_from_slice(&u64::to_le_bytes(v));
            }
            Ty::Bool => out.push(value.as_bool().ok_or(err("not a bool"))? as u8),
            Ty::Bytes(n) => {
                let start = out.len();
                decode_hex(value, out).ok_or(err("not hex"))?;
                if out.len() - start != *n {
                    return Err(err("wrong length"));
                }
            }
            Ty::ByteList => decode_hex(value, out).ok_or(err("not hex"))?,
            Ty::Vector(elem, n) => {
                let items = value.as_array().ok_or(err("not an array"))?;
                if items.len() != *n {
                    return Err(err("wrong length"));
                }
                for item in items {
                    elem.encode(field, item, out)?;
                }
            }
            Ty::List(elem) => {
                let items = value.as_array().ok_or(err("not an array"))?;
                if elem.fixed_size().is_some() {
                    for item in items {
                        elem.encode(field, item, out)?;
                    }
                } else {
                    let start = out.len();
                    out.resize(start + 4 * items.len(), 0);
                    for (i, item) in items.iter().enumerate() {
                        let offset = out.len() - start;
                        write_offset(out, start + 4 * i, offset);
                        elem.encode(field, item, out)?;
                    }
                }
            }
            Ty::Container(fields) => {
                let object = value.as_object().ok_or(err("not an object"))?;
                let start = out.len();
                let mut offsets = [0usize; 16];
                for (i, (name, ty)) in fields.iter().enumerate() {
                    let value = object
                        .get(*name)
                        .ok_or(JsonSszError::Field { field: name, reason: "missing" })?;
                    if ty.fixed_size().is_some() {
                        ty.encode(name, value, out)?;
                    } else {
                        offsets[i] = out.len();
                        out.extend_from_slice(&[0; 4]);
                    }
                }
                for (i, (name, ty)) in fields.iter().enumerate() {
                    if ty.fixed_size().is_none() {
                        let offset = out.len() - start;
                        write_offset(out, offsets[i], offset);
                        ty.encode(name, &object[*name], out)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Ty {
    pub fn decode(&self, field: &'static str, bytes: &[u8]) -> Result<Value, JsonSszError> {
        let err = |reason| JsonSszError::Field { field, reason };
        Ok(match self {
            Ty::U64 => Value::String(
                u64::from_le_bytes(bytes.try_into().map_err(|_| err("wrong length"))?).to_string(),
            ),
            Ty::Bool => match bytes {
                [0] => Value::Bool(false),
                [1] => Value::Bool(true),
                _ => return Err(err("not a bool")),
            },
            Ty::Bytes(n) if bytes.len() != *n => return Err(err("wrong length")),
            Ty::Bytes(_) | Ty::ByteList => Value::String(encode_hex(bytes)),
            Ty::Vector(elem, n) => {
                let size = elem.fixed_size().ok_or(err("variable vector element"))?;
                if bytes.len() != size * n {
                    return Err(err("wrong length"));
                }
                Value::Array(
                    bytes
                        .chunks_exact(size)
                        .map(|item| elem.decode(field, item))
                        .collect::<Result<_, _>>()?,
                )
            }
            Ty::List(elem) => match elem.fixed_size() {
                Some(size) => {
                    if !bytes.len().is_multiple_of(size) {
                        return Err(err("wrong length"));
                    }
                    Value::Array(
                        bytes
                            .chunks_exact(size)
                            .map(|item| elem.decode(field, item))
                            .collect::<Result<_, _>>()?,
                    )
                }
                None if bytes.is_empty() => Value::Array(Vec::new()),
                None => {
                    let count = read_offset(bytes, 0).ok_or(err("bad offset"))? / 4;
                    let mut items = Vec::with_capacity(count);
                    for i in 0..count {
                        let start = read_offset(bytes, 4 * i).ok_or(err("bad offset"))?;
                        let end = if i + 1 < count {
                            read_offset(bytes, 4 * (i + 1)).ok_or(err("bad offset"))?
                        } else {
                            bytes.len()
                        };
                        let item = bytes.get(start..end).ok_or(err("bad offset"))?;
                        items.push(elem.decode(field, item)?);
                    }
                    Value::Array(items)
                }
            },
            Ty::Container(fields) => {
                let mut object = serde_json::Map::with_capacity(fields.len());
                let mut at = 0;
                let mut variable: [(usize, usize); 16] = [(0, 0); 16];
                let mut variable_count = 0;
                for (i, (name, ty)) in fields.iter().enumerate() {
                    match ty.fixed_size() {
                        Some(size) => {
                            let item = bytes.get(at..at + size).ok_or(err("too short"))?;
                            object.insert(name.to_string(), ty.decode(name, item)?);
                            at += size;
                        }
                        None => {
                            let offset = read_offset(bytes, at).ok_or(err("bad offset"))?;
                            variable[variable_count] = (i, offset);
                            variable_count += 1;
                            at += 4;
                        }
                    }
                }
                for v in 0..variable_count {
                    let (i, start) = variable[v];
                    let end = if v + 1 < variable_count { variable[v + 1].1 } else { bytes.len() };
                    let (name, ty) = &fields[i];
                    let item = bytes.get(start..end).ok_or(err("bad offset"))?;
                    object.insert(name.to_string(), ty.decode(name, item)?);
                }
                Value::Object(object)
            }
        })
    }
}

fn read_offset(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize)
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(2 + 2 * bytes.len());
    hex.push_str("0x");
    for byte in bytes {
        hex.push(DIGITS[(byte >> 4) as usize] as char);
        hex.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    hex
}

fn write_offset(out: &mut [u8], at: usize, offset: usize) {
    out[at..at + 4].copy_from_slice(&(offset as u32).to_le_bytes());
}

fn decode_hex(value: &Value, out: &mut Vec<u8>) -> Option<()> {
    let hex = value.as_str()?.strip_prefix("0x")?.as_bytes();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    out.reserve(hex.len() / 2);
    for pair in hex.chunks_exact(2) {
        out.push(nibble(pair[0])? << 4 | nibble(pair[1])?);
    }
    Some(())
}

const ROOT: Ty = Ty::Bytes(32);
const PUBKEY: Ty = Ty::Bytes(48);
const SIGNATURE: Ty = Ty::Bytes(96);
const ADDRESS: Ty = Ty::Bytes(20);

const CHECKPOINT: Ty = Ty::Container(&[("epoch", Ty::U64), ("root", ROOT)]);
const ATTESTATION_DATA: Ty = Ty::Container(&[
    ("slot", Ty::U64),
    ("index", Ty::U64),
    ("beacon_block_root", ROOT),
    ("source", CHECKPOINT),
    ("target", CHECKPOINT),
]);
const BEACON_BLOCK_HEADER: Ty = Ty::Container(&[
    ("slot", Ty::U64),
    ("proposer_index", Ty::U64),
    ("parent_root", ROOT),
    ("state_root", ROOT),
    ("body_root", ROOT),
]);
const SIGNED_BEACON_BLOCK_HEADER: Ty =
    Ty::Container(&[("message", BEACON_BLOCK_HEADER), ("signature", SIGNATURE)]);
const PROPOSER_SLASHING: Ty = Ty::Container(&[
    ("signed_header_1", SIGNED_BEACON_BLOCK_HEADER),
    ("signed_header_2", SIGNED_BEACON_BLOCK_HEADER),
]);
const INDEXED_ATTESTATION: Ty = Ty::Container(&[
    ("attesting_indices", Ty::List(&Ty::U64)),
    ("data", ATTESTATION_DATA),
    ("signature", SIGNATURE),
]);
const ATTESTER_SLASHING: Ty = Ty::Container(&[
    ("attestation_1", INDEXED_ATTESTATION),
    ("attestation_2", INDEXED_ATTESTATION),
]);
const ATTESTATION: Ty = Ty::Container(&[
    ("aggregation_bits", Ty::ByteList),
    ("data", ATTESTATION_DATA),
    ("signature", SIGNATURE),
    ("committee_bits", Ty::Bytes(8)),
]);
const DEPOSIT: Ty = Ty::Container(&[
    ("proof", Ty::Vector(&ROOT, 33)),
    (
        "data",
        Ty::Container(&[
            ("pubkey", PUBKEY),
            ("withdrawal_credentials", ROOT),
            ("amount", Ty::U64),
            ("signature", SIGNATURE),
        ]),
    ),
]);
const SIGNED_VOLUNTARY_EXIT: Ty = Ty::Container(&[
    ("message", Ty::Container(&[("epoch", Ty::U64), ("validator_index", Ty::U64)])),
    ("signature", SIGNATURE),
]);
const SYNC_AGGREGATE: Ty = Ty::Container(&[
    ("sync_committee_bits", Ty::Bytes(64)),
    ("sync_committee_signature", SIGNATURE),
]);
const SIGNED_BLS_TO_EXECUTION_CHANGE: Ty = Ty::Container(&[
    (
        "message",
        Ty::Container(&[
            ("validator_index", Ty::U64),
            ("from_bls_pubkey", PUBKEY),
            ("to_execution_address", ADDRESS),
        ]),
    ),
    ("signature", SIGNATURE),
]);
pub const SIGNED_EXECUTION_PAYLOAD_BID: Ty = Ty::Container(&[
    (
        "message",
        Ty::Container(&[
            ("parent_block_hash", ROOT),
            ("parent_block_root", ROOT),
            ("block_hash", ROOT),
            ("prev_randao", ROOT),
            ("fee_recipient", ADDRESS),
            ("gas_limit", Ty::U64),
            ("builder_index", Ty::U64),
            ("slot", Ty::U64),
            ("value", Ty::U64),
            ("execution_payment", Ty::U64),
            ("blob_kzg_commitments", Ty::List(&Ty::Bytes(48))),
            ("execution_requests_root", ROOT),
        ]),
    ),
    ("signature", SIGNATURE),
]);
const PAYLOAD_ATTESTATION: Ty = Ty::Container(&[
    ("aggregation_bits", Ty::Bytes(64)),
    (
        "data",
        Ty::Container(&[
            ("beacon_block_root", ROOT),
            ("slot", Ty::U64),
            ("payload_present", Ty::Bool),
            ("blob_data_available", Ty::Bool),
        ]),
    ),
    ("signature", SIGNATURE),
]);
const EXECUTION_REQUESTS: Ty = Ty::Container(&[
    (
        "deposits",
        Ty::List(&Ty::Container(&[
            ("pubkey", PUBKEY),
            ("withdrawal_credentials", ROOT),
            ("amount", Ty::U64),
            ("signature", SIGNATURE),
            ("index", Ty::U64),
        ])),
    ),
    (
        "withdrawals",
        Ty::List(&Ty::Container(&[
            ("source_address", ADDRESS),
            ("validator_pubkey", PUBKEY),
            ("amount", Ty::U64),
        ])),
    ),
    (
        "consolidations",
        Ty::List(&Ty::Container(&[
            ("source_address", ADDRESS),
            ("source_pubkey", PUBKEY),
            ("target_pubkey", PUBKEY),
        ])),
    ),
    (
        "builder_deposits",
        Ty::List(&Ty::Container(&[
            ("pubkey", PUBKEY),
            ("withdrawal_credentials", ROOT),
            ("amount", Ty::U64),
            ("signature", SIGNATURE),
        ])),
    ),
    ("builder_exits", Ty::List(&Ty::Container(&[("source_address", ADDRESS), ("pubkey", PUBKEY)]))),
]);
const BEACON_BLOCK_BODY_GLOAS: Ty = Ty::Container(&[
    ("randao_reveal", SIGNATURE),
    (
        "eth1_data",
        Ty::Container(&[("deposit_root", ROOT), ("deposit_count", Ty::U64), ("block_hash", ROOT)]),
    ),
    ("graffiti", ROOT),
    ("proposer_slashings", Ty::List(&PROPOSER_SLASHING)),
    ("attester_slashings", Ty::List(&ATTESTER_SLASHING)),
    ("attestations", Ty::List(&ATTESTATION)),
    ("deposits", Ty::List(&DEPOSIT)),
    ("voluntary_exits", Ty::List(&SIGNED_VOLUNTARY_EXIT)),
    ("sync_aggregate", SYNC_AGGREGATE),
    ("bls_to_execution_changes", Ty::List(&SIGNED_BLS_TO_EXECUTION_CHANGE)),
    ("signed_execution_payload_bid", SIGNED_EXECUTION_PAYLOAD_BID),
    ("payload_attestations", Ty::List(&PAYLOAD_ATTESTATION)),
    ("parent_execution_requests", EXECUTION_REQUESTS),
]);
pub const SIGNED_BEACON_BLOCK_GLOAS: Ty = Ty::Container(&[
    (
        "message",
        Ty::Container(&[
            ("slot", Ty::U64),
            ("proposer_index", Ty::U64),
            ("parent_root", ROOT),
            ("state_root", ROOT),
            ("body", BEACON_BLOCK_BODY_GLOAS),
        ]),
    ),
    ("signature", SIGNATURE),
]);

pub fn to_json(ty: &Ty, ssz: &[u8]) -> Result<Value, JsonSszError> {
    ty.decode("root", ssz)
}

pub fn to_ssz(ty: &Ty, json: &[u8]) -> Result<Vec<u8>, JsonSszError> {
    let value: Value = serde_json::from_slice(json)?;
    let mut out = Vec::with_capacity(json.len() / 2);
    ty.encode("root", &value, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use ssz::Encode;

    use super::*;
    use crate::SignedBeaconBlockGloas;

    fn hex(len: usize, byte: u8) -> String {
        format!("0x{}", format!("{byte:02x}").repeat(len))
    }

    fn sig() -> String {
        format!("0xc0{}", "00".repeat(95))
    }

    fn checkpoint(n: u8) -> Value {
        json!({ "epoch": n.to_string(), "root": hex(32, n) })
    }

    fn attestation_data() -> Value {
        json!({
            "slot": "5", "index": "0", "beacon_block_root": hex(32, 1),
            "source": checkpoint(2), "target": checkpoint(3)
        })
    }

    fn signed_header() -> Value {
        json!({
            "message": {
                "slot": "7", "proposer_index": "8", "parent_root": hex(32, 4),
                "state_root": hex(32, 5), "body_root": hex(32, 6)
            },
            "signature": sig()
        })
    }

    fn indexed_attestation() -> Value {
        json!({ "attesting_indices": ["1", "2", "3"], "data": attestation_data(), "signature": sig() })
    }

    fn signed_bid() -> Value {
        json!({
            "message": {
                "parent_block_hash": hex(32, 0x20), "parent_block_root": hex(32, 0x21),
                "block_hash": hex(32, 0x22), "prev_randao": hex(32, 0x23),
                "fee_recipient": hex(20, 0x24), "gas_limit": "60000000", "builder_index": "2",
                "slot": "123", "value": "0", "execution_payment": "15044274",
                "blob_kzg_commitments": [hex(48, 0x25), hex(48, 0x26)],
                "execution_requests_root": hex(32, 0x27)
            },
            "signature": sig()
        })
    }

    #[test]
    fn a_signed_bid_decodes_to_the_same_json_as_lighthouse() {
        let bid = serde_json::from_value::<crate::SignedExecutionPayloadBid>(signed_bid()).unwrap();

        let json = to_json(&SIGNED_EXECUTION_PAYLOAD_BID, &bid.as_ssz_bytes()).unwrap();

        assert_eq!(json, serde_json::to_value(&bid).unwrap());
    }

    #[test]
    fn a_gloas_block_transcodes_to_the_same_ssz_as_lighthouse() {
        let block = json!({
            "message": {
                "slot": "123", "proposer_index": "45", "parent_root": hex(32, 0x10),
                "state_root": hex(32, 0x11),
                "body": {
                    "randao_reveal": sig(),
                    "eth1_data": { "deposit_root": hex(32, 0x13), "deposit_count": "9", "block_hash": hex(32, 0x14) },
                    "graffiti": hex(32, 0x15),
                    "proposer_slashings": [{ "signed_header_1": signed_header(), "signed_header_2": signed_header() }],
                    "attester_slashings": [{ "attestation_1": indexed_attestation(), "attestation_2": indexed_attestation() }],
                    "attestations": [{
                        "aggregation_bits": "0x0f", "data": attestation_data(),
                        "signature": sig(), "committee_bits": "0x0100000000000000"
                    }],
                    "deposits": [{
                        "proof": vec![hex(32, 0x17); 33],
                        "data": { "pubkey": hex(48, 0x18), "withdrawal_credentials": hex(32, 0x19), "amount": "32000000000", "signature": sig() }
                    }],
                    "voluntary_exits": [{ "message": { "epoch": "3", "validator_index": "4" }, "signature": sig() }],
                    "sync_aggregate": { "sync_committee_bits": hex(64, 0xff), "sync_committee_signature": sig() },
                    "bls_to_execution_changes": [{
                        "message": { "validator_index": "6", "from_bls_pubkey": hex(48, 0x1d), "to_execution_address": hex(20, 0x1e) },
                        "signature": sig()
                    }],
                    "signed_execution_payload_bid": signed_bid(),
                    "payload_attestations": [{
                        "aggregation_bits": hex(64, 0x01),
                        "data": { "beacon_block_root": hex(32, 0x29), "slot": "122", "payload_present": true, "blob_data_available": false },
                        "signature": sig()
                    }],
                    "parent_execution_requests": {
                        "deposits": [{ "pubkey": hex(48, 0x2b), "withdrawal_credentials": hex(32, 0x2c), "amount": "1", "signature": sig(), "index": "0" }],
                        "withdrawals": [{ "source_address": hex(20, 0x2e), "validator_pubkey": hex(48, 0x2f), "amount": "2" }],
                        "consolidations": [{ "source_address": hex(20, 0x30), "source_pubkey": hex(48, 0x31), "target_pubkey": hex(48, 0x32) }],
                        "builder_deposits": [{ "pubkey": hex(48, 0x33), "withdrawal_credentials": hex(32, 0x34), "amount": "1000000000", "signature": sig() }],
                        "builder_exits": [{ "source_address": hex(20, 0x36), "pubkey": hex(48, 0x37) }]
                    }
                }
            },
            "signature": sig()
        });
        let json = serde_json::to_vec(&block).unwrap();

        let expected = serde_json::from_slice::<SignedBeaconBlockGloas>(&json)
            .expect("lighthouse decodes the fixture")
            .as_ssz_bytes();

        assert_eq!(to_ssz(&SIGNED_BEACON_BLOCK_GLOAS, &json).unwrap(), expected);
    }
}
