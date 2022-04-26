use sbor::*;
use scrypto::buffer::scrypto_encode;
use scrypto::crypto::hash;
use scrypto::rust::ops::RangeFull;
use scrypto::engine::types::*;
use scrypto::rust::collections::*;
use scrypto::rust::vec::Vec;
use crate::engine::track::PhysicalSubstateId;

use crate::ledger::*;


pub struct CommitReceipt {
    pub down_substates: HashSet<(Hash, u32)>,
    pub up_substates: Vec<(Hash, u32)>,
}

impl CommitReceipt {
    fn new() -> Self {
        CommitReceipt {
            down_substates: HashSet::new(),
            up_substates: Vec::new(),
        }
    }

    fn down(&mut self, id: (Hash, u32)) {
        self.down_substates.insert(id);
    }

    fn up(&mut self, id: (Hash, u32)) {
        self.up_substates.push(id);
    }
}

#[derive(Debug, Clone, TypeId, Encode, Decode, PartialEq, Eq)]
pub enum StateUpdateInstruction {
    Down(PhysicalSubstateId),
    Up(Vec<u8>, Vec<u8>),
}

#[derive(Debug, Clone, TypeId, Encode, Decode, PartialEq, Eq)]
pub struct StateUpdateReceipt {
    pub instructions: Vec<StateUpdateInstruction>,
}

impl StateUpdateReceipt {
    /// Commits changes to the underlying ledger.
    /// Currently none of these objects are deleted so all commits are puts
    pub fn commit<S: WriteableSubstateStore>(mut self, store: &mut S) -> CommitReceipt {
        let hash = hash(scrypto_encode(&self));
        let mut receipt = CommitReceipt::new();
        let mut id_gen = SubstateIdGenerator::new(hash);

        for instruction in self.instructions.drain(RangeFull) {
            match instruction {
                StateUpdateInstruction::Down(PhysicalSubstateId(hash, index)) => receipt.down((hash, index)),
                StateUpdateInstruction::Up(key, value) => {
                    let phys_id = id_gen.next();
                    receipt.up(phys_id);
                    store.put_keyed_substate(&key, value, phys_id);
                }
            }
        }

        receipt
    }
}