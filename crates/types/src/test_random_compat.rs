//! `TestRandom` used to be provided by lighthouse (`test_random_derive` +
//! `lh_types::test_utils::TestRandom`), but was removed upstream in favor of
//! `arbitrary`-based generation (lighthouse PR #9006). helix's own types still want a cheap
//! "give me a random instance" helper for round-trip tests, so this vendors the removed trait
//! and its blanket impls (unchanged from lighthouse's last version) rather than touching the
//! ~12 call sites across the crate. Impls for lighthouse's own types (`Epoch`, `Slot`) are
//! added here too, since those no longer come from upstream either.

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use lh_bls::{
    AggregateSignature, PUBLIC_KEY_BYTES_LEN, PublicKey, PublicKeyBytes, SIGNATURE_BYTES_LEN,
    SecretKey, Signature, SignatureBytes,
};
use lh_kzg::{BYTES_PER_COMMITMENT, KzgCommitment, KzgProof};
use lh_types::{
    ConsolidationRequest, DepositRequest, Epoch, EthSpec, ExecutionRequestsElectra, Slot,
    Withdrawal, WithdrawalRequest,
};
use rand::RngCore;
use ssz_types::{FixedVector, VariableList, typenum::Unsigned};

pub trait TestRandom {
    fn random_for_test(rng: &mut impl RngCore) -> Self;
}

impl TestRandom for bool {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        (rng.next_u32() % 2) == 1
    }
}

impl TestRandom for u8 {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        rng.next_u32().to_be_bytes()[0]
    }
}

impl TestRandom for u32 {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        rng.next_u32()
    }
}

impl TestRandom for u64 {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        rng.next_u64()
    }
}

impl TestRandom for usize {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        rng.next_u32() as usize
    }
}

impl<U: TestRandom> TestRandom for Vec<U> {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        (0..(usize::random_for_test(rng) % 4)).map(|_| U::random_for_test(rng)).collect()
    }
}

impl<U: TestRandom> TestRandom for Arc<U> {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Arc::new(U::random_for_test(rng))
    }
}

impl<U: TestRandom, const N: usize> TestRandom for smallvec::SmallVec<[U; N]> {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        (0..(usize::random_for_test(rng) % 4)).map(|_| U::random_for_test(rng)).collect()
    }
}

macro_rules! impl_test_random_for_u8_array {
    ($len:expr) => {
        impl TestRandom for [u8; $len] {
            fn random_for_test(rng: &mut impl RngCore) -> Self {
                let mut bytes = [0; $len];
                rng.fill_bytes(&mut bytes);
                bytes
            }
        }
    };
}

impl_test_random_for_u8_array!(3);
impl_test_random_for_u8_array!(4);
impl_test_random_for_u8_array!(32);
impl_test_random_for_u8_array!(48);
impl_test_random_for_u8_array!(96);

impl<T: TestRandom, N: Unsigned> TestRandom for FixedVector<T, N> {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Self::new((0..N::to_usize()).map(|_| T::random_for_test(rng)).collect())
            .expect("N items provided")
    }
}

impl<T: TestRandom, N: Unsigned> TestRandom for VariableList<T, N> {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        let len =
            if N::to_usize() == 0 { 0 } else { usize::random_for_test(rng) % 4.min(N::to_usize()) };
        (0..len).map(|_| T::random_for_test(rng)).collect::<Vec<_>>().try_into().unwrap()
    }
}

impl TestRandom for Address {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        let mut bytes = [0; 20];
        rng.fill_bytes(&mut bytes);
        Address::from(bytes)
    }
}

impl TestRandom for B256 {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        let mut bytes = [0; 32];
        rng.fill_bytes(&mut bytes);
        B256::from(bytes)
    }
}

impl TestRandom for U256 {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        let mut bytes = [0; 32];
        rng.fill_bytes(&mut bytes);
        U256::from_le_slice(&bytes)
    }
}

impl TestRandom for Epoch {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Epoch::new(u64::random_for_test(rng))
    }
}

impl TestRandom for Slot {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Slot::new(u64::random_for_test(rng))
    }
}

impl TestRandom for Withdrawal {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Self {
            index: u64::random_for_test(rng),
            validator_index: u64::random_for_test(rng),
            address: Address::random_for_test(rng),
            amount: u64::random_for_test(rng),
        }
    }
}

impl TestRandom for DepositRequest {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Self {
            pubkey: PublicKeyBytes::random_for_test(rng),
            withdrawal_credentials: B256::random_for_test(rng),
            amount: u64::random_for_test(rng),
            signature: SignatureBytes::random_for_test(rng),
            index: u64::random_for_test(rng),
        }
    }
}

impl TestRandom for WithdrawalRequest {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Self {
            source_address: Address::random_for_test(rng),
            validator_pubkey: PublicKeyBytes::random_for_test(rng),
            amount: u64::random_for_test(rng),
        }
    }
}

impl TestRandom for ConsolidationRequest {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Self {
            source_address: Address::random_for_test(rng),
            source_pubkey: PublicKeyBytes::random_for_test(rng),
            target_pubkey: PublicKeyBytes::random_for_test(rng),
        }
    }
}

impl<E: EthSpec> TestRandom for ExecutionRequestsElectra<E> {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        Self {
            deposits: TestRandom::random_for_test(rng),
            withdrawals: TestRandom::random_for_test(rng),
            consolidations: TestRandom::random_for_test(rng),
        }
    }
}

impl TestRandom for SecretKey {
    fn random_for_test(_rng: &mut impl RngCore) -> Self {
        SecretKey::random()
    }
}

impl TestRandom for PublicKey {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        SecretKey::random_for_test(rng).public_key()
    }
}

impl TestRandom for PublicKeyBytes {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        if bool::random_for_test(rng) {
            PublicKeyBytes::from(PublicKey::random_for_test(rng))
        } else {
            PublicKeyBytes::deserialize(&<[u8; PUBLIC_KEY_BYTES_LEN]>::random_for_test(rng))
                .unwrap()
        }
    }
}

impl TestRandom for Signature {
    fn random_for_test(_rng: &mut impl RngCore) -> Self {
        Signature::infinity().expect("infinity signature is valid")
    }
}

impl TestRandom for SignatureBytes {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        if bool::random_for_test(rng) {
            SignatureBytes::from(Signature::random_for_test(rng))
        } else {
            SignatureBytes::deserialize(&<[u8; SIGNATURE_BYTES_LEN]>::random_for_test(rng)).unwrap()
        }
    }
}

impl TestRandom for AggregateSignature {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        let mut aggregate = AggregateSignature::infinity();
        aggregate.add_assign(&Signature::random_for_test(rng));
        aggregate
    }
}

impl TestRandom for KzgCommitment {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        KzgCommitment(<[u8; 48]>::random_for_test(rng))
    }
}

impl TestRandom for KzgProof {
    fn random_for_test(rng: &mut impl RngCore) -> Self {
        let mut bytes = [0; BYTES_PER_COMMITMENT];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }
}
