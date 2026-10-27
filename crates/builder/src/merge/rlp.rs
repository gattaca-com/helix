//! RLP pieces shared by the list and state tries.

/// A node's reference in its parent: its hash, or its whole encoding when under 32 bytes.
#[derive(Clone, Copy, Default)]
pub struct Ref {
    pub len: u8,
    pub data: [u8; 32],
}

impl Ref {
    pub fn get(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

fn header_len(len: usize) -> usize {
    match len {
        0..56 => 1,
        _ => 1 + 8 - len.leading_zeros() as usize / 8,
    }
}

fn put_header(out: &mut Vec<u8>, base: u8, len: usize) {
    if len < 56 {
        out.push(base + len as u8);
        return;
    }
    let n = 8 - len.leading_zeros() as usize / 8;
    out.push(base + 55 + n as u8);
    out.extend_from_slice(&len.to_be_bytes()[8 - n..]);
}

pub fn put_list_header(out: &mut Vec<u8>, len: usize) {
    put_header(out, 0xc0, len);
}

pub fn bytes_len(bytes: &[u8]) -> usize {
    match bytes {
        [b] if *b < 0x80 => 1,
        _ => header_len(bytes.len()) + bytes.len(),
    }
}

pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    if let [b] = bytes &&
        *b < 0x80
    {
        out.push(*b);
        return;
    }
    put_header(out, 0x80, bytes.len());
    out.extend_from_slice(bytes);
}

/// Hex-prefix encoded `path`, as an RLP string.
pub fn compact_len(path: &[u8]) -> usize {
    let n = path.len() / 2 + 1;
    // The flag nibble is at most 3, so a lone compact byte is its own RLP string.
    if n == 1 { 1 } else { 1 + n }
}

pub fn put_compact(out: &mut Vec<u8>, path: &[u8], leaf: bool) {
    let flag = 2 * u8::from(leaf) + (path.len() % 2) as u8;
    let n = path.len() / 2 + 1;
    let (first, rest) = match path.len() % 2 {
        1 => ((flag << 4) | path[0], &path[1..]),
        _ => (flag << 4, path),
    };
    if n == 1 {
        out.push(first);
        return;
    }
    out.push(0x80 + n as u8);
    out.push(first);
    out.extend(rest.chunks_exact(2).map(|p| (p[0] << 4) | p[1]));
}

pub fn ref_len(r: &Ref) -> usize {
    match r.len {
        32 => 33,
        len => len as usize,
    }
}

pub fn put_ref(out: &mut Vec<u8>, r: &Ref) {
    if r.len == 32 {
        out.push(0xa0);
    }
    out.extend_from_slice(r.get());
}
