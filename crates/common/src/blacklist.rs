use alloy_primitives::Address;
use dashmap::DashSet;
use sha2::{Digest, Sha256};

/// Parses a blacklist payload into the addresses to disallow.
pub fn parse_disallow_list(list: Vec<String>) -> Vec<Address> {
    let mut addrs = Vec::new();
    for hex in list {
        if let Ok(addr) = hex.strip_prefix("0x").unwrap_or(&hex).parse::<Address>() {
            addrs.push(addr);
        }
    }
    addrs
}

/// Fingerprints the disallow list so operators can confirm every node enforces the same one.
///
/// Entries are sorted first because `DashSet` iteration order is not stable. Mirrors reth's
/// `hash_disallow_list` so the digests are comparable against reth-based builders.
pub fn hash_disallow_list(disallow: &DashSet<Address>) -> String {
    let mut sorted: Vec<Address> = disallow.iter().map(|addr| *addr).collect();
    sorted.sort_unstable();

    let mut hasher = Sha256::new();
    for addr in &sorted {
        hasher.update(addr.as_slice());
    }

    format!("{:x}", hasher.finalize())
}

/// Returns the disallow list's digest, or `None` when it still matches `previous`.
pub fn changed_disallow_hash(
    disallow: &DashSet<Address>,
    previous: Option<&str>,
) -> Option<String> {
    let hash = hash_disallow_list(disallow);
    (previous != Some(hash.as_str())).then_some(hash)
}

#[cfg(test)]
mod blacklist_tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn loads_an_address_list() {
        let parsed = parse_disallow_list(vec![
            "0x8589427373D6D84E98730D7795D8f6f8731FDA16".into(),
            "722122dF12D4e14e13Ac3b6895a86e84145b6967".into(),
            "0xdd4c48c0b24039969fc16d1cdf626eab821d3384".into(),
        ]);

        assert_eq!(parsed.len(), 3, "every entry must load");
        assert!(parsed.contains(&address!("0x8589427373D6D84E98730D7795D8f6f8731FDA16")));
    }

    const ADDR_A: Address = address!("0x722122dF12D4e14e13Ac3b6895a86e84145b6967");
    const ADDR_B: Address = address!("0x8589427373D6D84E98730D7795D8f6f8731FDA16");

    fn disallow_set(addrs: &[Address]) -> DashSet<Address> {
        let set = DashSet::new();
        for addr in addrs {
            set.insert(*addr);
        }
        set
    }

    #[test]
    fn an_unchanged_list_reports_no_change() {
        let previous = disallow_set(&[ADDR_A, ADDR_B]);
        let current = disallow_set(&[ADDR_B, ADDR_A]);

        let hash = changed_disallow_hash(&previous, None).expect("a first list is always new");

        assert_eq!(changed_disallow_hash(&current, Some(&hash)), None);
    }

    #[test]
    fn an_amended_list_reports_a_new_hash() {
        let previous = disallow_set(&[ADDR_A]);
        let current = disallow_set(&[ADDR_A, ADDR_B]);

        let superseded =
            changed_disallow_hash(&previous, None).expect("a first list is always new");

        let in_force =
            changed_disallow_hash(&current, Some(&superseded)).expect("the digest must change");
        assert_ne!(in_force, superseded);
    }
}
