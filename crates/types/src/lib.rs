use lh_types::MainnetEthSpec;

pub type Withdrawal = lh_types::withdrawal::Withdrawal;
pub type Withdrawals = lh_types::execution_payload::Withdrawals<MainnetEthSpec>;

pub fn eth_consensus_pubkey_to_alloy(
    pubkey: &ethereum_consensus::primitives::BlsPublicKey,
) -> alloy_rpc_types::beacon::BlsPublicKey {
    alloy_rpc_types::beacon::BlsPublicKey::from_slice(pubkey.as_ref())
}

pub fn alloy_pubkey_to_eth_consensus(
    pubkey: &alloy_rpc_types::beacon::BlsPublicKey,
) -> ethereum_consensus::primitives::BlsPublicKey {
    ethereum_consensus::primitives::BlsPublicKey::try_from(pubkey.as_ref()).unwrap()
}

pub fn eth_consensus_hash_to_alloy(
    hash: &ethereum_consensus::primitives::Bytes32,
) -> alloy_primitives::B256 {
    alloy_primitives::B256::from_slice(hash.as_ref())
}

pub fn alloy_hash_to_eth_consensus(
    hash: &alloy_primitives::B256,
) -> ethereum_consensus::primitives::Bytes32 {
    ethereum_consensus::primitives::Bytes32::try_from(hash.as_ref()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eth_consensus_pubkey_to_alloy() {
        let pubkey = alloy_rpc_types::beacon::BlsPublicKey::random();
        let pubkey_2 = eth_consensus_pubkey_to_alloy(&alloy_pubkey_to_eth_consensus(&pubkey));
        assert_eq!(pubkey, pubkey_2);
    }

    #[test]
    fn test_eth_consensus_hash_to_alloy() {
        let hash = alloy_primitives::B256::random();
        let hash_2 = eth_consensus_hash_to_alloy(&alloy_hash_to_eth_consensus(&hash));
        assert_eq!(hash, hash_2);
    }
}
