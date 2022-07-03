use std::path::PathBuf;

use radix_engine::ledger::*;
use rocksdb::{DBWithThreadMode, Direction, IteratorMode, SingleThreaded, DB};
use sbor::Decode;
use scrypto::buffer::*;
use scrypto::engine::types::*;

pub struct RadixEngineDB {
    db: DBWithThreadMode<SingleThreaded>,
}

impl RadixEngineDB {
    pub fn new(root: PathBuf) -> Self {
        let db = DB::open_default(root.as_path()).unwrap();
        Self { db }
    }

    pub fn with_bootstrap(root: PathBuf) -> Self {
        let mut substate_store = Self::new(root);
        bootstrap(&mut substate_store);
        substate_store
    }

    pub fn list_packages(&self) -> Vec<PackageAddress> {
        let start = &scrypto_encode(&PackageAddress([0; 26]));
        let end = &scrypto_encode(&PackageAddress([255; 26]));
        self.list_items(start, end)
    }

    pub fn list_components(&self) -> Vec<ComponentAddress> {
        let start = &scrypto_encode(&ComponentAddress([0; 26]));
        let end = &scrypto_encode(&ComponentAddress([255; 26]));
        self.list_items(start, end)
    }

    pub fn list_resource_managers(&self) -> Vec<ResourceAddress> {
        let start = &scrypto_encode(&ResourceAddress([0; 26]));
        let end = &scrypto_encode(&ResourceAddress([255; 26]));
        self.list_items(start, end)
    }

    fn list_items<T: Decode>(&self, start: &[u8], inclusive_end: &[u8]) -> Vec<T> {
        let mut iter = self
            .db
            .iterator(IteratorMode::From(start, Direction::Forward));
        let mut items = Vec::new();
        while let Some(kv) = iter.next() {
            if kv.0.as_ref() > inclusive_end {
                break;
            }
            if kv.0.len() == start.len() {
                items.push(scrypto_decode(kv.0.as_ref()).unwrap());
            }
        }
        items
    }

    fn read(&self, key: &[u8]) -> Option<Vec<u8>> {
        // TODO: Use get_pinned
        self.db.get(key).unwrap()
    }

    fn write(&self, key: &[u8], value: &[u8]) {
        self.db.put(key, value).unwrap();
    }
}


impl ReadableSubstateStore for RadixEngineDB {
    fn get_substate(&self, address: &[u8]) -> Option<Substate> {
        self.read(address).map(|b| scrypto_decode(&b).unwrap())
    }

    fn get_space(&mut self, address: &[u8]) -> Option<PhysicalSubstateId> {
        self.read(&address).map(|b| scrypto_decode(&b).unwrap())
    }

    fn get_epoch(&self) -> u64 {
        let id = scrypto_encode(&"epoch");
        self.read(&id)
            .map(|v| scrypto_decode(&v).unwrap())
            .unwrap_or(0)
    }
}

impl WriteableSubstateStore for RadixEngineDB {
    fn put_substate(&mut self, address: &[u8], substate: Substate) {
        self.write(address, &scrypto_encode(&substate));
    }

    fn put_space(&mut self, address: &[u8], phys_id: PhysicalSubstateId) {
        self.write(&address, &scrypto_encode(&phys_id));
    }

    fn set_epoch(&mut self, epoch: u64) {
        let id = scrypto_encode(&"epoch");
        let value = scrypto_encode(&epoch);
        self.write(&id, &value)
    }
}

impl QueryableSubstateStore for RadixEngineDB {
    fn get_substates(
        &self,
        address: &[u8],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let key_size = address.len();
        let mut iter = self
            .db
            .iterator(IteratorMode::From(address, Direction::Forward));
        iter.next(); // Key Value Store
        let mut items = Vec::new();
        while let Some((key, value)) = iter.next() {
            if !key.starts_with(address) {
                break;
            }

            let local_key = key.split_at(key_size).1.to_vec();
            let substate: Substate = scrypto_decode(&value.to_vec()).unwrap();
            items.push((local_key, substate.value));
        }
        items
    }
}
