use std::collections::BTreeMap;

use helix_types::Slot;

use crate::auctioneer::types::SeenBlock;

#[derive(Debug, PartialEq, Eq)]
pub enum Recorded {
    New,
    /// Already recorded, e.g. from the proposer and then our beacon node.
    Repeat,
    /// The slot's attestation deadline has already been handled.
    Late,
}

/// The distinct blocks seen per slot, whichever builder they name, decided once at the
/// slot's attestation deadline. More than one block for a slot is the proposer
/// equivocating.
#[derive(Default)]
pub struct SeenBlocks {
    slots: BTreeMap<Slot, Vec<SeenBlock>>,
    decided: Slot,
}

impl SeenBlocks {
    pub fn record(&mut self, slot: Slot, block: SeenBlock) -> Recorded {
        if slot <= self.decided {
            return Recorded::Late;
        }
        let seen = self.slots.entry(slot).or_default();
        if seen.iter().any(|b| b.block_root == block.block_root) {
            return Recorded::Repeat;
        }
        seen.push(block);
        Recorded::New
    }

    /// Closes `slot`: later blocks for it are late. Returns what was seen for it.
    pub fn decide(&mut self, slot: Slot) -> Vec<SeenBlock> {
        self.decided = self.decided.max(slot);
        let seen = self.slots.remove(&slot).unwrap_or_default();
        self.slots.retain(|kept, _| *kept > slot);
        seen
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    fn block(byte: u8) -> SeenBlock {
        SeenBlock {
            block_root: B256::repeat_byte(byte),
            parent_root: B256::ZERO,
            bid_builder_index: 1,
            bid_block_hash: B256::repeat_byte(0xaa),
        }
    }

    #[test]
    fn the_same_block_twice_is_one_block() {
        let mut seen = SeenBlocks::default();
        let slot = Slot::new(10);
        assert_eq!(seen.record(slot, block(1)), Recorded::New);
        assert_eq!(seen.record(slot, block(1)), Recorded::Repeat);
        assert_eq!(seen.decide(slot), vec![block(1)]);
    }

    #[test]
    fn every_distinct_block_for_a_slot_is_kept() {
        let mut seen = SeenBlocks::default();
        let slot = Slot::new(10);
        seen.record(slot, block(1));
        seen.record(slot, block(2));
        seen.record(Slot::new(11), block(3));
        assert_eq!(seen.decide(slot), vec![block(1), block(2)]);
        assert_eq!(seen.decide(Slot::new(11)), vec![block(3)]);
    }

    #[test]
    fn a_block_after_its_deadline_is_late() {
        let mut seen = SeenBlocks::default();
        let slot = Slot::new(10);
        assert!(seen.decide(slot).is_empty());
        assert_eq!(seen.record(slot, block(1)), Recorded::Late);
        assert_eq!(seen.record(Slot::new(9), block(2)), Recorded::Late);
        assert_eq!(seen.record(Slot::new(11), block(3)), Recorded::New);
    }

    /// A slot whose deadline event never came is dropped once a later slot is decided.
    #[test]
    fn undecided_earlier_slots_are_dropped() {
        let mut seen = SeenBlocks::default();
        seen.record(Slot::new(10), block(1));
        seen.decide(Slot::new(11));
        assert!(seen.slots.is_empty());
    }
}
