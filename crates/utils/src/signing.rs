use ethereum_consensus::{
    crypto::SecretKey,
    domains::DomainType,
    phase0::mainnet::compute_domain,
    primitives::{BlsPublicKey, BlsSignature, Domain, Root, Slot},
    signing::{compute_signing_root, sign_with_domain, verify_signed_data},
    ssz::prelude::*,
    state_transition::Context,
    Error, Fork,
};

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
