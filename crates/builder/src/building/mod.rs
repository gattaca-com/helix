//! The building role: builds a block for the next slot and submits it to the
//! relay. Shares the embedded ethrex node with the other roles.

mod keys;

pub use keys::BuildingKeys;
