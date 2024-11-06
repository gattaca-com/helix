use ethereum_consensus::{
    crypto::SecretKey,
    domains::DomainType,
    phase0::mainnet::compute_domain,
    primitives::{BlsPublicKey, BlsSignature, Domain, Root, Slot},
    signing::{compute_signing_root, sign_with_domain, verify_signature, verify_signed_data},
    ssz::prelude::*,
    state_transition::Context,
    Error, Fork,
};
use tree_hash::TreeHash;
use tree_hash_derive::TreeHash;

pub const APPLICATION_BUILDER_DOMAIN: [u8; 4] = [0, 0, 0, 1];
pub const GENESIS_VALIDATORS_ROOT: [u8; 32] = [0; 32];
pub const COMMIT_BOOST_DOMAIN: [u8; 4] = [109, 109, 111, 67];

pub fn verify_signed_consensus_message<T: Merkleized>(
    message: &mut T,
    signature: &BlsSignature,
    public_key: &BlsPublicKey,
    context: &Context,
    slot_hint: Option<Slot>,
    root_hint: Option<Root>,
) -> Result<(), Error> {
    let fork_version = slot_hint.map(|slot| match context.fork_for(slot) {
        Fork::Bellatrix => context.bellatrix_fork_version,
        Fork::Capella => context.capella_fork_version,
        Fork::Deneb => context.deneb_fork_version,
        _ => unimplemented!("Fork {:?} is not supported", context.fork_for(slot)),
    });
    let domain =
        compute_domain(DomainType::BeaconProposer, fork_version, root_hint, context).unwrap();
    verify_signed_data(message, signature, public_key, domain)?;
    Ok(())
}

pub fn verify_signed_builder_message<T: Merkleized>(
    message: &mut T,
    signature: &BlsSignature,
    public_key: &BlsPublicKey,
    context: &Context,
) -> Result<(), Error> {
    let domain = compute_builder_domain(context)?;
    verify_signed_data(message, signature, public_key, domain)?;
    Ok(())
}

pub fn compute_signing_root_custom(object_root: [u8; 32], signing_domain: [u8; 32]) -> [u8; 32] {
    #[derive(Default, Debug, TreeHash)]
    struct SigningData {
        object_root: [u8; 32],
        signing_domain: [u8; 32],
    }

    let signing_data = SigningData { object_root, signing_domain };
    signing_data.tree_hash_root().0
}

pub fn verify_signed_message<T: TreeHash>(
    message: &T,
    signature: &BlsSignature,
    public_key: &BlsPublicKey,
    domain_mask: [u8; 4],
    context: &Context,
) -> Result<(), Error> {
    let domain = compute_domain_custom(context, domain_mask);
    let signing_root = compute_signing_root_custom(message.tree_hash_root().0, domain);

    verify_signature(public_key, &signing_root, signature)
}

// NOTE: this currently works only for builder domain signatures and
// verifications
// ref: https://github.com/ralexstokes/ethereum-consensus/blob/cf3c404043230559660810bc0c9d6d5a8498d819/ethereum-consensus/src/builder/mod.rs#L26-L29
pub fn compute_domain_custom(chain: &Context, domain_mask: [u8; 4]) -> [u8; 32] {
    #[derive(Debug, TreeHash)]
    struct ForkData {
        fork_version: [u8; 4],
        genesis_validators_root: [u8; 32],
    }

    let mut domain = [0u8; 32];
    domain[..4].copy_from_slice(&domain_mask);

    let fork_version = chain.genesis_fork_version;
    let fd = ForkData { fork_version, genesis_validators_root: GENESIS_VALIDATORS_ROOT };
    let fork_data_root = fd.tree_hash_root().0;

    domain[4..].copy_from_slice(&fork_data_root[..28]);

    domain
}

pub fn compute_consensus_signing_root<T: Merkleized>(
    data: &mut T,
    slot: Slot,
    genesis_validators_root: &Root,
    context: &Context,
) -> Result<Root, Error> {
    let fork = context.fork_for(slot);
    let fork_version = context.fork_version_for(fork);
    let domain = compute_domain(
        DomainType::BeaconProposer,
        Some(fork_version),
        Some(*genesis_validators_root),
        context,
    )?;
    compute_signing_root(data, domain)
}

pub fn sign_builder_message<T: Merkleized>(
    message: &mut T,
    signing_key: &SecretKey,
    context: &Context,
) -> Result<BlsSignature, Error> {
    let domain = compute_builder_domain(context)?;
    sign_with_domain(message, signing_key, domain)
}

pub fn compute_builder_signing_root<T: Merkleized>(
    data: &mut T,
    context: &Context,
) -> Result<Root, Error> {
    let domain = compute_builder_domain(context)?;
    compute_signing_root(data, domain)
}

pub fn compute_builder_domain(context: &Context) -> Result<Domain, Error> {
    let domain_type = DomainType::ApplicationBuilder;
    compute_domain(domain_type, None, None, context)
}
