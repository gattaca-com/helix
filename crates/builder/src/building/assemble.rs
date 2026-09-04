use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use ethrex_blockchain::{
    Blockchain,
    payload::{BuildPayloadArgs, HeadTransaction, PayloadBuildContext, create_payload},
};
use ethrex_common::{
    U256 as EU256,
    types::{
        AccountUpdate, BlobsBundle, Block, ELASTICITY_MULTIPLIER, MempoolTransaction, Transaction,
        requests::EncodedRequests,
    },
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_rlp::encode::RLPEncode;
use ethrex_storage::Store;
use thiserror::Error;

use crate::{
    building::slot::SlotContext,
    config::BuildingConfig,
    engine::convert::{au256, eaddr, eu256, h256},
};

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("parent block not found")]
    MissingParent,
    /// Nothing to bid: the relay rejects a zero-value block.
    #[error("no payout: tips and subsidy do not cover the payout gas")]
    NoPayout,
    #[error("the builder cannot afford the payout")]
    PayoutUnaffordable,
    #[error("the payout transaction reverted")]
    PayoutReverted,
    #[error("the payout recipient is the builder itself")]
    PayoutToSelf,
    #[error("an Amsterdam block was built without a block access list")]
    MissingBlockAccessList,
    #[error("build failed: {0}")]
    Internal(String),
}

/// A finalized block and the bid it backs.
#[derive(Debug)]
pub struct BuiltBlock {
    pub block: Block,
    // These three are read by the submission.
    #[allow(dead_code)]
    pub blobs_bundle: BlobsBundle,
    #[allow(dead_code)]
    pub requests: Vec<EncodedRequests>,
    /// The changed accounts, for checking the payment the way the relay does.
    #[allow(dead_code)]
    pub account_updates: Vec<AccountUpdate>,
    /// The encoded EIP-7928 list, from Amsterdam onwards. The header commits to
    /// these exact bytes, so the submission has to carry them unchanged.
    pub block_access_list: Option<Vec<u8>>,
    /// Paid to the proposer by the trailing transaction, and the value the
    /// `BidTrace` claims.
    pub value: U256,
}

/// Builds a block for `slot` from the node's mempool, ending with a transfer
/// that pays the proposer.
///
/// The coinbase is the builder, so tips accrue here and the bid is funded from
/// them plus the configured subsidy.
pub fn build(
    store: &Store,
    blockchain: &Blockchain,
    slot: &SlotContext,
    config: &BuildingConfig,
    payout_signer: &PrivateKeySigner,
    chain_id: u64,
) -> Result<BuiltBlock, BuildError> {
    let builder = payout_signer.address();
    if builder == slot.proposer_fee_recipient {
        return Err(BuildError::PayoutToSelf);
    }
    if store
        .get_block_header_by_hash(h256(slot.parent_hash))
        .map_err(|e| BuildError::Internal(e.to_string()))?
        .is_none()
    {
        return Err(BuildError::MissingParent);
    }

    // EIP-7843 puts the proposal slot in the header, and only from Amsterdam:
    // setting it earlier would change every pre-Amsterdam block hash.
    let is_amsterdam = store.get_chain_config().is_amsterdam_activated(slot.timestamp);

    let args = BuildPayloadArgs {
        parent: h256(slot.parent_hash),
        timestamp: slot.timestamp,
        fee_recipient: eaddr(builder),
        random: h256(slot.prev_randao),
        withdrawals: Some(slot.withdrawals.iter().map(ewithdrawal_lh).collect()),
        beacon_root: Some(h256(slot.parent_beacon_block_root)),
        slot_number: is_amsterdam.then_some(slot.slot),
        version: 3,
        elasticity_multiplier: ELASTICITY_MULTIPLIER,
        // `create_payload` runs this through `calc_gas_limit`, which applies
        // the 1/1024 clamp against the parent.
        gas_ceil: slot.registered_gas_limit,
    };
    let template = create_payload(&args, store, config.extra_data.clone().into())
        .map_err(|e| BuildError::Internal(format!("create_payload: {e}")))?;

    let mut ctx = PayloadBuildContext::new(template, store, &blockchain.options.r#type)
        .map_err(|e| BuildError::Internal(format!("payload context: {e}")))?;
    blockchain
        .apply_system_operations(&mut ctx)
        .map_err(|e| BuildError::Internal(format!("system operations: {e}")))?;

    // `fill_transactions` spends every last drop of `remaining_gas`, so hold
    // the payout's share back and restore it once the fill is done. The reserve
    // is sized before the fill and the transaction after it, so a recipient the
    // fill creates is not charged for twice.
    let reserve = payout_gas_limit(config, is_amsterdam, recipient_exists(&mut ctx, slot)?)
        .min(ctx.remaining_gas);
    ctx.remaining_gas -= reserve;
    blockchain
        .fill_transactions(&mut ctx)
        .map_err(|e| BuildError::Internal(format!("fill transactions: {e}")))?;
    ctx.remaining_gas += reserve;

    let payout_gas = payout_gas_limit(config, is_amsterdam, recipient_exists(&mut ctx, slot)?);
    let base_fee = ctx.payload.header.base_fee_per_gas.unwrap_or_default();
    let payout = payout_value(&ctx, config, payout_gas, base_fee)?;

    let nonce = ctx
        .vm
        .db
        .get_account(eaddr(builder))
        .map_err(|e| BuildError::Internal(e.to_string()))?
        .info
        .nonce;
    let balance = ctx
        .vm
        .db
        .get_account(eaddr(builder))
        .map_err(|e| BuildError::Internal(e.to_string()))?
        .info
        .balance;
    let gas_cost = EU256::from(payout_gas) * EU256::from(base_fee);
    if balance < eu256(payout) + gas_cost {
        return Err(BuildError::PayoutUnaffordable);
    }

    let payout_tx = signed_payout(
        payout_signer,
        chain_id,
        nonce,
        slot.proposer_fee_recipient,
        payout,
        payout_gas,
        base_fee as u128,
    )?;
    let sender = payout_tx
        .sender(&NativeCrypto)
        .map_err(|e| BuildError::Internal(format!("payout sender: {e}")))?;
    blockchain
        .apply_tx_to_payload(
            HeadTransaction { tx: MempoolTransaction::new(payout_tx, sender), tip: EU256::zero() },
            &mut ctx,
        )
        .map_err(|e| BuildError::Internal(format!("payout tx: {e}")))?;
    if !ctx.receipts.last().is_some_and(|receipt| receipt.succeeded) {
        return Err(BuildError::PayoutReverted);
    }

    blockchain
        .extract_requests(&mut ctx)
        .map_err(|e| BuildError::Internal(format!("extract requests: {e}")))?;
    blockchain
        .apply_withdrawals(&mut ctx)
        .map_err(|e| BuildError::Internal(format!("apply withdrawals: {e}")))?;
    blockchain
        .finalize_payload(&mut ctx)
        .map_err(|e| BuildError::Internal(format!("finalize: {e}")))?;

    // `finalize_payload` hashed this into the header, so the submission has to
    // carry the same encoding rather than re-deriving one.
    let block_access_list = ctx.block_access_list.as_ref().map(|bal| bal.encode_to_vec());
    if is_amsterdam && block_access_list.is_none() {
        return Err(BuildError::MissingBlockAccessList);
    }

    Ok(BuiltBlock {
        block: ctx.payload,
        blobs_bundle: ctx.blobs_bundle,
        requests: ctx.requests.unwrap_or_default(),
        account_updates: ctx.account_updates,
        block_access_list,
        value: payout,
    })
}

/// Whether the payout recipient already has an account, read from in-block
/// state so a payment earlier in the same block counts.
fn recipient_exists(ctx: &mut PayloadBuildContext, slot: &SlotContext) -> Result<bool, BuildError> {
    Ok(ctx
        .vm
        .db
        .get_account(eaddr(slot.proposer_fee_recipient))
        .map_err(|e| BuildError::Internal(e.to_string()))?
        .info !=
        Default::default())
}

/// The gas the payout transaction needs. `payout_gas_reserve` covers the
/// transfer; paying an address for the first time also creates it, and from
/// Amsterdam that costs state gas (EIP-8037). A fee recipient's first payment
/// is exactly that case, and it is the normal case on a fresh testnet, so the
/// reserve has to cover it or every block is lost.
fn payout_gas_limit(config: &BuildingConfig, is_amsterdam: bool, recipient_exists: bool) -> u64 {
    if is_amsterdam && !recipient_exists {
        config.payout_gas_reserve + new_account_state_gas()
    } else {
        config.payout_gas_reserve
    }
}

/// EIP-8037's charge for the state a new account occupies. Read from ethrex so
/// it cannot drift from the rules the simulator enforces.
fn new_account_state_gas() -> u64 {
    use ethrex_levm::gas_cost::{STATE_BYTES_PER_NEW_ACCOUNT, cost_per_state_byte};
    // `cost_per_state_byte` ignores its argument at this ethrex revision.
    STATE_BYTES_PER_NEW_ACCOUNT * cost_per_state_byte(0)
}

/// Tips earned plus the subsidy, less the gas the payout itself will burn.
fn payout_value(
    ctx: &PayloadBuildContext,
    config: &BuildingConfig,
    payout_gas: u64,
    base_fee: u64,
) -> Result<U256, BuildError> {
    let gas_cost = EU256::from(payout_gas) * EU256::from(base_fee);
    let funded = ctx.block_value + EU256::from(config.subsidy_wei);
    let payout = funded.checked_sub(gas_cost).ok_or(BuildError::NoPayout)?;
    if payout.is_zero() {
        return Err(BuildError::NoPayout);
    }
    Ok(au256(payout))
}

fn signed_payout(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    gas_limit: u64,
    base_fee: u128,
) -> Result<Transaction, BuildError> {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas: base_fee,
        // A tip would pay the builder out of its own payout, and the relay's
        // payment check refuses one.
        max_priority_fee_per_gas: 0,
        to: to.into(),
        value,
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| BuildError::Internal(format!("payout signature: {e}")))?;
    let encoded = alloy_consensus::TxEnvelope::from(tx.into_signed(signature)).encoded_2718();
    Transaction::decode_canonical(&encoded)
        .map_err(|e| BuildError::Internal(format!("payout decode: {e}")))
}

/// The consensus withdrawal type, not the alloy one `convert` bridges.
fn ewithdrawal_lh(w: &helix_types::Withdrawal) -> ethrex_common::types::Withdrawal {
    ethrex_common::types::Withdrawal {
        index: w.index,
        validator_index: w.validator_index,
        address: eaddr(w.address),
        amount: w.amount,
    }
}

#[cfg(test)]
mod tests;
