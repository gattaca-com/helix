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

    let args = BuildPayloadArgs {
        parent: h256(slot.parent_hash),
        timestamp: slot.timestamp,
        fee_recipient: eaddr(builder),
        random: h256(slot.prev_randao),
        withdrawals: Some(slot.withdrawals.iter().map(ewithdrawal_lh).collect()),
        beacon_root: Some(h256(slot.parent_beacon_block_root)),
        slot_number: None,
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
    // the payout's share back and restore it once the fill is done.
    let reserve = config.payout_gas_reserve.min(ctx.remaining_gas);
    ctx.remaining_gas -= reserve;
    blockchain
        .fill_transactions(&mut ctx)
        .map_err(|e| BuildError::Internal(format!("fill transactions: {e}")))?;
    ctx.remaining_gas += reserve;

    let base_fee = ctx.payload.header.base_fee_per_gas.unwrap_or_default();
    let payout = payout_value(&ctx, config, base_fee)?;

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
    let gas_cost = EU256::from(config.payout_gas_reserve) * EU256::from(base_fee);
    if balance < eu256(payout) + gas_cost {
        return Err(BuildError::PayoutUnaffordable);
    }

    let payout_tx = signed_payout(
        payout_signer,
        chain_id,
        nonce,
        slot.proposer_fee_recipient,
        payout,
        config.payout_gas_reserve,
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

    Ok(BuiltBlock {
        block: ctx.payload,
        blobs_bundle: ctx.blobs_bundle,
        requests: ctx.requests.unwrap_or_default(),
        account_updates: ctx.account_updates,
        value: payout,
    })
}

/// Tips earned plus the subsidy, less the gas the payout itself will burn.
fn payout_value(
    ctx: &PayloadBuildContext,
    config: &BuildingConfig,
    base_fee: u64,
) -> Result<U256, BuildError> {
    let gas_cost = EU256::from(config.payout_gas_reserve) * EU256::from(base_fee);
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
