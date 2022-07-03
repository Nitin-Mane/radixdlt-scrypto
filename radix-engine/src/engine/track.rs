use indexmap::{IndexMap, IndexSet};
use sbor::rust::collections::*;
use sbor::rust::format;
use sbor::rust::ops::RangeFull;
use sbor::rust::string::String;
use sbor::rust::vec::Vec;
use sbor::rust::vec;
use sbor::*;
use scrypto::buffer::scrypto_decode;
use scrypto::buffer::scrypto_encode;
use scrypto::engine::types::*;
use scrypto::values::ScryptoValue;
use transaction::validation::*;

use crate::engine::track::BorrowedSubstate::Taken;
use crate::engine::{REValue, SubstateOperation, SubstateOperationsReceipt};
use crate::ledger::*;
use crate::model::*;

enum BorrowedSubstate {
    Loaded(SubstateValue, u32),
    LoadedMut(SubstateValue),
    Taken,
}

impl BorrowedSubstate {
    fn loaded(value: SubstateValue, mutable: bool) -> Self {
        if mutable {
            BorrowedSubstate::LoadedMut(value)
        } else {
            BorrowedSubstate::Loaded(value, 1)
        }
    }
}

/// Facilitates transactional state updates.
pub struct Track<'s, S: ReadableSubstateStore> {
    substate_store: &'s mut S,
    transaction_hash: Hash,
    id_allocator: IdAllocator,
    logs: Vec<(Level, String)>,

    new_addresses: Vec<Address>,
    borrowed_substates: HashMap<Address, BorrowedSubstate>,

    downed_substates: Vec<PhysicalSubstateId>,
    down_virtual_substates: Vec<VirtualSubstateId>,
    up_substates: IndexMap<Vec<u8>, SubstateValue>,
    up_virtual_substate_space: IndexSet<Vec<u8>>,
}

#[derive(Debug)]
pub enum TrackError {
    Reentrancy,
    NotFound,
}

pub struct BorrowedSNodes {
    borrowed_substates: HashSet<Address>,
}

impl BorrowedSNodes {
    pub fn is_empty(&self) -> bool {
        self.borrowed_substates.is_empty()
    }
}

pub struct TrackReceipt {
    pub borrowed: BorrowedSNodes,
    pub new_addresses: Vec<Address>,
    pub logs: Vec<(Level, String)>,
    pub substates: SubstateOperationsReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubstateUpdate<T> {
    pub prev_id: Option<PhysicalSubstateId>,
    pub value: T,
}

#[derive(Debug, Clone, TypeId, Encode, Decode, PartialEq, Eq)]
pub enum SubstateParentId {
    Exists(PhysicalSubstateId),
    New(usize),
}

#[derive(Debug, Clone, TypeId, Encode, Decode, PartialEq, Eq)]
pub struct VirtualSubstateId(pub SubstateParentId, pub Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AddressPath {
    ValueId(ValueId),
}

impl AddressPath {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            AddressPath::ValueId(value_id) => value_id.encode_address(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Address {
    Resource(ResourceAddress),
    GlobalComponent(ComponentAddress),
    Package(PackageAddress),
    NonFungibleSet(ResourceAddress),

    KeyValueStore(Vec<AddressPath>, KeyValueStoreId),
    Vault(Vec<AddressPath>, VaultId),
    LocalComponent(Vec<AddressPath>, ComponentAddress),
}

#[derive(Debug)]
pub enum SubstateValue {
    Resource(ResourceManager),
    Component(Component),
    Package(ValidatedPackage),
    Vault(Vault),
    NonFungible(Option<NonFungible>),
    KeyValueStoreEntry(Option<Vec<u8>>),
}

// TODO: Replace NonFungible with real re address
// TODO: Move this logic into application layer
macro_rules! resource_to_non_fungible_space {
    ($resource_address:expr) => {{
        let mut addr = scrypto_encode(&$resource_address);
        addr.push(0u8);
        addr
    }};
}

impl Address {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Address::Resource(resource_address) => scrypto_encode(resource_address),
            Address::GlobalComponent(component_address) => scrypto_encode(component_address),
            Address::Package(package_address) => scrypto_encode(package_address),
            Address::NonFungibleSet(resource_address) => {
                resource_to_non_fungible_space!(resource_address.clone())
            }
            Address::KeyValueStore(ancestors, kv_store_id) => {
                let mut address = Vec::new();
                for ancestor in ancestors {
                    address.extend(ancestor.encode());
                }
                address.extend(scrypto_encode(kv_store_id));
                address
            }
            Address::Vault(ancestors, vault_id) => {
                let mut address = Vec::new();
                for ancestor in ancestors {
                    address.extend(ancestor.encode());
                }
                address.extend(scrypto_encode(vault_id));
                address
            }
            Address::LocalComponent(ancestors, child_id) => {
                let mut address = Vec::new();
                for ancestor in ancestors {
                    address.extend(ancestor.encode());
                }
                address.extend(scrypto_encode(child_id));
                address
            }
        }
    }

    pub fn child(&self, child_id: ValueId) -> Address {
        let next_ancestors = match self {
            Address::KeyValueStore(ancestors, kv_store_id) => {
                let mut next_ancestors = ancestors.clone();
                next_ancestors.push(AddressPath::ValueId(ValueId::KeyValueStore(kv_store_id.clone())));
                next_ancestors
            }
            Address::LocalComponent(ancestors, component_id) => {
                let mut next_ancestors = ancestors.clone();
                next_ancestors.push(AddressPath::ValueId(ValueId::Component(component_id.clone())));
                next_ancestors
            }
            Address::GlobalComponent(component_address) => vec![AddressPath::ValueId(ValueId::Component(*component_address))],
            _ => panic!("Unexpected"),
        };

        match child_id {
            ValueId::KeyValueStore(kv_store_id) => Address::KeyValueStore(next_ancestors, kv_store_id),
            ValueId::Vault(vault_id) => Address::Vault(next_ancestors, vault_id),
            ValueId::Component(component_id) => Address::LocalComponent(next_ancestors, component_id),
            _ => panic!("Unexpected"),
        }
    }
}

impl Into<Address> for PackageAddress {
    fn into(self) -> Address {
        Address::Package(self)
    }
}

impl Into<Address> for ComponentAddress {
    fn into(self) -> Address {
        Address::GlobalComponent(self)
    }
}

impl Into<Address> for ResourceAddress {
    fn into(self) -> Address {
        Address::Resource(self)
    }
}

impl Into<PackageAddress> for Address {
    fn into(self) -> PackageAddress {
        if let Address::Package(package_address) = self {
            return package_address;
        } else {
            panic!("Address is not a package address");
        }
    }
}

impl Into<ComponentAddress> for Address {
    fn into(self) -> ComponentAddress {
        if let Address::GlobalComponent(component_address) = self {
            return component_address;
        } else {
            panic!("Address is not a component address");
        }
    }
}

impl Into<ResourceAddress> for Address {
    fn into(self) -> ResourceAddress {
        if let Address::Resource(resource_address) = self {
            return resource_address;
        } else {
            panic!("Address is not a resource address");
        }
    }
}

impl SubstateValue {
    fn encode(&self) -> Vec<u8> {
        match self {
            SubstateValue::Resource(resource_manager) => scrypto_encode(resource_manager),
            SubstateValue::Package(package) => scrypto_encode(package),
            SubstateValue::Component(component) => scrypto_encode(component),
            SubstateValue::Vault(vault) => scrypto_encode(vault),
            SubstateValue::NonFungible(non_fungible) => scrypto_encode(non_fungible),
            SubstateValue::KeyValueStoreEntry(value) => scrypto_encode(value),
        }
    }

    pub fn vault_mut(&mut self) -> &mut Vault {
        if let SubstateValue::Vault(vault) = self {
            vault
        } else {
            panic!("Not a vault");
        }
    }

    pub fn vault(&self) -> &Vault {
        if let SubstateValue::Vault(vault) = self {
            vault
        } else {
            panic!("Not a vault");
        }
    }

    pub fn resource_manager_mut(&mut self) -> &mut ResourceManager {
        if let SubstateValue::Resource(resource_manager) = self {
            resource_manager
        } else {
            panic!("Not a resource manager");
        }
    }

    pub fn resource_manager(&self) -> &ResourceManager {
        if let SubstateValue::Resource(resource_manager) = self {
            resource_manager
        } else {
            panic!("Not a resource manager");
        }
    }

    pub fn component(&self) -> &Component {
        if let SubstateValue::Component(component) = self {
            component
        } else {
            panic!("Not a component");
        }
    }

    pub fn component_mut(&mut self) -> &mut Component {
        if let SubstateValue::Component(component) = self {
            component
        } else {
            panic!("Not a component");
        }
    }

    pub fn package(&self) -> &ValidatedPackage {
        if let SubstateValue::Package(package) = self {
            package
        } else {
            panic!("Not a package");
        }
    }

    pub fn non_fungible(&self) -> &Option<NonFungible> {
        if let SubstateValue::NonFungible(non_fungible) = self {
            non_fungible
        } else {
            panic!("Not a NonFungible");
        }
    }

    pub fn kv_entry(&self) -> &Option<Vec<u8>> {
        if let SubstateValue::KeyValueStoreEntry(kv_entry) = self {
            kv_entry
        } else {
            panic!("Not a KVEntry");
        }
    }
}

impl Into<SubstateValue> for ValidatedPackage {
    fn into(self) -> SubstateValue {
        SubstateValue::Package(self)
    }
}

impl Into<SubstateValue> for Component {
    fn into(self) -> SubstateValue {
        SubstateValue::Component(self)
    }
}

impl Into<SubstateValue> for ResourceManager {
    fn into(self) -> SubstateValue {
        SubstateValue::Resource(self)
    }
}

impl Into<SubstateValue> for Vault {
    fn into(self) -> SubstateValue {
        SubstateValue::Vault(self)
    }
}

impl Into<SubstateValue> for Option<NonFungible> {
    fn into(self) -> SubstateValue {
        SubstateValue::NonFungible(self)
    }
}

impl Into<SubstateValue> for Option<ScryptoValue> {
    fn into(self) -> SubstateValue {
        SubstateValue::KeyValueStoreEntry(self.map(|v| v.raw))
    }
}

impl Into<Component> for SubstateValue {
    fn into(self) -> Component {
        if let SubstateValue::Component(component) = self {
            component
        } else {
            panic!("Not a component");
        }
    }
}

impl Into<ResourceManager> for SubstateValue {
    fn into(self) -> ResourceManager {
        if let SubstateValue::Resource(resource_manager) = self {
            resource_manager
        } else {
            panic!("Not a resource manager");
        }
    }
}

impl Into<Vault> for SubstateValue {
    fn into(self) -> Vault {
        if let SubstateValue::Vault(vault) = self {
            vault
        } else {
            panic!("Not a vault");
        }
    }
}

impl<'s, S: ReadableSubstateStore> Track<'s, S> {
    pub fn new(substate_store: &'s mut S, transaction_hash: Hash) -> Self {
        Self {
            substate_store,
            transaction_hash,
            id_allocator: IdAllocator::new(IdSpace::Application),
            logs: Vec::new(),

            new_addresses: Vec::new(),
            borrowed_substates: HashMap::new(),

            downed_substates: Vec::new(),
            down_virtual_substates: Vec::new(),
            up_substates: IndexMap::new(),
            up_virtual_substate_space: IndexSet::new(),
        }
    }

    /// Returns the transaction hash.
    pub fn transaction_hash(&self) -> Hash {
        self.transaction_hash
    }

    /// Returns the current epoch.
    pub fn current_epoch(&self) -> u64 {
        self.substate_store.get_epoch()
    }

    /// Adds a log message.
    pub fn add_log(&mut self, level: Level, message: String) {
        self.logs.push((level, message));
    }

    /// Creates a row with the given key/value
    pub fn create_uuid_value<A: Into<Address>, V: Into<SubstateValue>>(
        &mut self,
        addr: A,
        value: V,
    ) {
        let address = addr.into();
        self.new_addresses.push(address.clone());
        self.up_substates.insert(address.encode(), value.into());
    }

    // TODO: Make more generic
    pub fn create_non_fungible_space(&mut self, resource_address: ResourceAddress) {
        let space_address = resource_to_non_fungible_space!(resource_address);
        self.up_virtual_substate_space.insert(space_address);
    }

    pub fn create_key_space(
        &mut self,
        component_address: ComponentAddress,
        kv_store_id: KeyValueStoreId,
    ) {
        let mut space_address = scrypto_encode(&component_address);
        space_address.extend(scrypto_encode(&kv_store_id));
        self.up_virtual_substate_space.insert(space_address);
    }

    pub fn create_key_space_2(&mut self, address: Address) {
        self.up_virtual_substate_space.insert(address.encode());
    }

    pub fn take_lock<A: Into<Address>>(
        &mut self,
        addr: A,
        mutable: bool,
    ) -> Result<(), TrackError> {
        let address = addr.into();
        let maybe_value = self.up_substates.remove(&address.encode());
        if let Some(value) = maybe_value {
            self.borrowed_substates
                .insert(address, BorrowedSubstate::loaded(value, mutable));
            return Ok(());
        }

        if let Some(current) = self.borrowed_substates.get_mut(&address) {
            if mutable {
                return Err(TrackError::Reentrancy);
            } else {
                match current {
                    BorrowedSubstate::Taken | BorrowedSubstate::LoadedMut(..) => {
                        panic!("Should never get here")
                    }
                    BorrowedSubstate::Loaded(_, ref mut count) => *count = *count + 1,
                }
                return Ok(());
            }
        }

        if let Some(substate) = self.substate_store.get_substate(&address.encode()) {
            self.downed_substates.push(substate.phys_id);
            let value = match address {
                Address::GlobalComponent(_) | Address::LocalComponent(..) => {
                    let component = scrypto_decode(&substate.value).unwrap();
                    SubstateValue::Component(component)
                }
                Address::Resource(_) => {
                    let resource_manager = scrypto_decode(&substate.value).unwrap();
                    SubstateValue::Resource(resource_manager)
                }
                Address::Vault(..) => {
                    let vault = scrypto_decode(&substate.value).unwrap();
                    SubstateValue::Vault(vault)
                }
                Address::Package(..) => {
                    let package = scrypto_decode(&substate.value).unwrap();
                    SubstateValue::Package(package)
                }
                _ => panic!("Attempting to borrow unsupported value {:?}", address),
            };

            self.borrowed_substates
                .insert(address.clone(), BorrowedSubstate::loaded(value, mutable));
            Ok(())
        } else {
            Err(TrackError::NotFound)
        }
    }

    pub fn read_value<A: Into<Address>>(&self, addr: A) -> &SubstateValue {
        let address: Address = addr.into();
        match self
            .borrowed_substates
            .get(&address)
            .expect(&format!("{:?} was never locked", address))
        {
            BorrowedSubstate::LoadedMut(value) => value,
            BorrowedSubstate::Loaded(value, ..) => value,
            BorrowedSubstate::Taken => panic!("Value was already taken"),
        }
    }

    pub fn take_value<A: Into<Address>>(&mut self, addr: A) -> SubstateValue {
        let address: Address = addr.into();
        match self
            .borrowed_substates
            .insert(address.clone(), Taken)
            .expect(&format!("{:?} was never locked", address))
        {
            BorrowedSubstate::LoadedMut(value) => value,
            BorrowedSubstate::Loaded(..) => panic!("Cannot take value on immutable: {:?}", address),
            BorrowedSubstate::Taken => panic!("Value was already taken"),
        }
    }

    pub fn write_value<A: Into<Address>, V: Into<SubstateValue>>(&mut self, addr: A, value: V) {
        let address: Address = addr.into();

        let cur_value = self
            .borrowed_substates
            .get(&address)
            .expect("value was never locked");
        match cur_value {
            BorrowedSubstate::Loaded(..) => panic!("Cannot write to immutable"),
            BorrowedSubstate::LoadedMut(..) | BorrowedSubstate::Taken => {}
        }

        self.borrowed_substates
            .insert(address, BorrowedSubstate::LoadedMut(value.into()));
    }

    // TODO: Replace with more generic write_value once Component is split into more substates
    pub fn write_component_value(&mut self, address: Address, value: Vec<u8>) {
        match address {
            Address::GlobalComponent(..) | Address::LocalComponent(..) => {}
            _ => panic!("Unexpected address"),
        }

        let borrowed = self
            .borrowed_substates
            .get_mut(&address)
            .expect("Value was never locked");
        match borrowed {
            BorrowedSubstate::Taken => panic!("Value was taken"),
            BorrowedSubstate::Loaded(..) => panic!("Cannot write to immutable"),
            BorrowedSubstate::LoadedMut(component_val) => {
                component_val.component_mut().set_state(value);
            }
        }
    }

    pub fn release_lock<A: Into<Address>>(&mut self, addr: A) {
        let address = addr.into();
        let borrowed = self
            .borrowed_substates
            .remove(&address)
            .expect("Value was never borrowed");
        match borrowed {
            BorrowedSubstate::Taken => panic!("Value was never returned"),
            BorrowedSubstate::LoadedMut(value) => {
                self.up_substates.insert(address.encode(), value);
            }
            BorrowedSubstate::Loaded(value, mut count) => {
                count = count - 1;
                if count == 0 {
                    self.up_substates.insert(address.encode(), value);
                } else {
                    self.borrowed_substates
                        .insert(address, BorrowedSubstate::Loaded(value, count));
                }
            }
        }
    }

    /// Returns the value of a key value pair
    pub fn read_key_value(&mut self, parent_address: Address, key: Vec<u8>) -> SubstateValue {
        let mut address = parent_address.encode();
        address.extend(key);
        if let Some(cur) = self.up_substates.get(&address) {
            match cur {
                SubstateValue::KeyValueStoreEntry(e) => {
                    return SubstateValue::KeyValueStoreEntry(e.clone())
                }
                SubstateValue::NonFungible(n) => return SubstateValue::NonFungible(n.clone()),
                _ => panic!("Unsupported key value"),
            }
        }
        match parent_address {
            Address::NonFungibleSet(_) => self
                .substate_store
                .get_substate(&address)
                .map(|r| {
                    let non_fungible = scrypto_decode(&r.value).unwrap();
                    SubstateValue::NonFungible(non_fungible)
                })
                .unwrap_or(SubstateValue::NonFungible(None)),
            Address::KeyValueStore(..) => self
                .substate_store
                .get_substate(&address)
                .map(|r| {
                    let kv_store_entry = scrypto_decode(&r.value).unwrap();
                    SubstateValue::KeyValueStoreEntry(kv_store_entry)
                })
                .unwrap_or(SubstateValue::KeyValueStoreEntry(None)),
            _ => panic!("Invalid keyed value address {:?}", parent_address),
        }
    }

    /// Sets a key value
    pub fn set_key_value<V: Into<SubstateValue>>(
        &mut self,
        parent_address: Address,
        key: Vec<u8>,
        value: V,
    ) {
        let mut address = parent_address.encode();
        address.extend(key.clone());

        if self.up_substates.remove(&address).is_none() {
            let cur: Option<Substate> = self.substate_store.get_substate(&address);
            if let Some(Substate { value: _, phys_id }) = cur {
                self.downed_substates.push(phys_id);
            } else {
                let parent_id = self.get_substate_parent_id(&parent_address.encode());
                let virtual_substate_id = VirtualSubstateId(parent_id, key);
                self.down_virtual_substates.push(virtual_substate_id);
            }
        };

        self.up_substates.insert(address, value.into());
    }

    fn get_substate_parent_id(&mut self, space_address: &[u8]) -> SubstateParentId {
        if let Some(index) = self.up_virtual_substate_space.get_index_of(space_address) {
            SubstateParentId::New(index)
        } else {
            let substate_id = self.substate_store.get_space(space_address).unwrap();
            SubstateParentId::Exists(substate_id)
        }
    }

    /// Creates a new package ID.
    pub fn new_package_address(&mut self) -> PackageAddress {
        // Security Alert: ensure ID allocating will practically never fail
        let package_address = self
            .id_allocator
            .new_package_address(self.transaction_hash())
            .unwrap();
        package_address
    }

    /// Creates a new component address.
    pub fn new_component_address(&mut self) -> ComponentAddress {
        let component_address = self
            .id_allocator
            .new_component_address(self.transaction_hash())
            .unwrap();
        component_address
    }

    /// Creates a new resource address.
    pub fn new_resource_address(&mut self) -> ResourceAddress {
        let resource_address = self
            .id_allocator
            .new_resource_address(self.transaction_hash())
            .unwrap();
        resource_address
    }

    /// Creates a new UUID.
    pub fn new_uuid(&mut self) -> u128 {
        self.id_allocator.new_uuid(self.transaction_hash()).unwrap()
    }

    /// Creates a new bucket ID.
    pub fn new_bucket_id(&mut self) -> BucketId {
        self.id_allocator.new_bucket_id().unwrap()
    }

    /// Creates a new vault ID.
    pub fn new_vault_id(&mut self) -> VaultId {
        self.id_allocator
            .new_vault_id(self.transaction_hash())
            .unwrap()
    }

    /// Creates a new reference id.
    pub fn new_proof_id(&mut self) -> ProofId {
        self.id_allocator.new_proof_id().unwrap()
    }

    /// Creates a new map id.
    pub fn new_kv_store_id(&mut self) -> KeyValueStoreId {
        self.id_allocator
            .new_kv_store_id(self.transaction_hash())
            .unwrap()
    }

    /// Commits changes to the underlying ledger.
    /// Currently none of these objects are deleted so all commits are puts
    pub fn to_receipt(mut self) -> TrackReceipt {
        let mut store_instructions = Vec::new();
        for substate_id in self.downed_substates {
            store_instructions.push(SubstateOperation::Down(substate_id));
        }
        for virtual_substate_id in self.down_virtual_substates {
            store_instructions.push(SubstateOperation::VirtualDown(virtual_substate_id));
        }
        for (address, value) in self.up_substates.drain(RangeFull) {
            store_instructions.push(SubstateOperation::Up(address, value.encode()));
        }
        for space_address in self.up_virtual_substate_space.drain(RangeFull) {
            store_instructions.push(SubstateOperation::VirtualUp(space_address));
        }

        let substates = SubstateOperationsReceipt {
            substate_operations: store_instructions,
        };
        let borrowed = BorrowedSNodes {
            borrowed_substates: self.borrowed_substates.into_keys().collect(),
        };
        TrackReceipt {
            new_addresses: self.new_addresses,
            borrowed,
            substates,
            logs: self.logs,
        }
    }

    pub fn insert_objects(
        &mut self,
        values: HashMap<ValueId, REValue>,
        address: Address,
    ) {
        for (id, value) in values {
            let child_address = address.child(id);
            match value {
                REValue::Vault(vault) => {
                    self.create_uuid_value(child_address, vault);
                }
                REValue::Component {
                    component,
                    child_values,
                } => {
                    self.create_uuid_value(child_address.clone(), component);
                    let child_values = child_values
                        .into_iter()
                        .map(|(id, v)| (id, v.into_inner()))
                        .collect();
                    self.insert_objects(child_values, child_address);
                }
                REValue::KeyValueStore {
                    store,
                    child_values,
                } => {
                    self.create_key_space_2(child_address.clone());
                    for (k, v) in store.store {
                        self.set_key_value(child_address.clone(), k, Some(v));
                    }

                    // TODO: Move child values with entry as parent rather than the store
                    let child_values = child_values
                        .into_iter()
                        .map(|(id, v)| (id, v.into_inner()))
                        .collect();
                    self.insert_objects(child_values, child_address);
                }
                _ => panic!("Invalid value being persisted: {:?}", value),
            }
        }
    }
}
