use sbor::rust::cell::{Ref, RefCell, RefMut};
use sbor::rust::collections::hash_map::IntoIter;
use sbor::rust::collections::*;
use sbor::rust::vec::Vec;
use sbor::rust::vec;
use scrypto::engine::types::*;
use scrypto::values::ScryptoValue;

use crate::engine::*;
use crate::model::*;

#[derive(Debug)]
pub enum REValue {
    Bucket(Bucket),
    Proof(Proof),
    Vault(Vault),
    KeyValueStore(PreCommittedKeyValueStore),
    Component {
        component: Component,
        child_values: InMemoryChildren,
    },
    Package(ValidatedPackage),
    Resource(ResourceManager),
    NonFungibles(HashMap<NonFungibleId, NonFungible>),
}

impl REValue {
    pub fn resource_manager(&self) -> &ResourceManager {
        match self {
            REValue::Resource(resource_manager) => resource_manager,
            _ => panic!("Expected to be a resource manager"),
        }
    }

    pub fn resource_manager_mut(&mut self) -> &mut ResourceManager {
        match self {
            REValue::Resource(resource_manager) => resource_manager,
            _ => panic!("Expected to be a resource manager"),
        }
    }

    pub fn non_fungibles(&self) -> &HashMap<NonFungibleId, NonFungible> {
        match self {
            REValue::NonFungibles(non_fungibles) => non_fungibles,
            _ => panic!("Expected to be non fungibles"),
        }
    }

    pub fn non_fungibles_mut(&mut self) -> &mut HashMap<NonFungibleId, NonFungible> {
        match self {
            REValue::NonFungibles(non_fungibles) => non_fungibles,
            _ => panic!("Expected to be non fungibles"),
        }
    }

    pub fn package(&self) -> &ValidatedPackage {
        match self {
            REValue::Package(package) => package,
            _ => panic!("Expected to be a package"),
        }
    }

    pub fn component(&self) -> &Component {
        match self {
            REValue::Component { component, .. } => component,
            _ => panic!("Expected to be a store"),
        }
    }

    pub fn component_mut(&mut self) -> &mut Component {
        match self {
            REValue::Component { component, .. } => component,
            _ => panic!("Expected to be a store"),
        }
    }

    pub fn kv_store(&self) -> &PreCommittedKeyValueStore {
        match self {
            REValue::KeyValueStore(store) => store,
            _ => panic!("Expected to be a store"),
        }
    }

    pub fn kv_store_mut(&mut self) -> &mut PreCommittedKeyValueStore {
        match self {
            REValue::KeyValueStore(store) => store,
            _ => panic!("Expected to be a store"),
        }
    }

    pub fn vault(&self) -> &Vault {
        match self {
            REValue::Vault(vault) => vault,
            _ => panic!("Expected to be a vault"),
        }
    }

    pub fn vault_mut(&mut self) -> &mut Vault {
        match self {
            REValue::Vault(vault) => vault,
            _ => panic!("Expected to be a vault"),
        }
    }

    pub fn all_descendants(&self) -> Vec<AddressPath> {
        match self {
            REValue::KeyValueStore(store) => store.all_descendants(),
            REValue::Component {
                component: _,
                child_values,
            } => child_values.all_descendants(),
            _ => vec![],
        }
    }

    pub unsafe fn get_child(&self, path: &[AddressPath]) -> Ref<REValue> {
        match self {
            REValue::KeyValueStore(store) => store.get_child(path),
            REValue::Component {
                component: _,
                child_values,
            } => child_values.get_child(path),
            _ => panic!("Unexpected"),
        }
    }

    pub unsafe fn get_child_mut(&mut self, path: &[AddressPath]) -> RefMut<REValue> {
        match self {
            REValue::KeyValueStore(store) => store.get_child_mut(path),
            REValue::Component {
                component: _,
                child_values,
            } => child_values.get_child_mut(path),
            _ => panic!("Unexpected"),
        }
    }

    pub unsafe fn insert_children(&mut self, values: HashMap<AddressPath, REValue>) {
        match self {
            REValue::KeyValueStore(store) => store.children.insert_children(values),
            REValue::Component {
                component: _,
                child_values,
            } => child_values.insert_children(values),
            _ => panic!("Unexpected"),
        }
    }

    pub fn verify_can_move(&self) -> Result<(), RuntimeError> {
        match self {
            REValue::Bucket(bucket) => {
                if bucket.is_locked() {
                    Err(RuntimeError::CantMoveLockedBucket)
                } else {
                    Ok(())
                }
            }
            REValue::Proof(proof) => {
                if proof.is_restricted() {
                    Err(RuntimeError::CantMoveRestrictedProof)
                } else {
                    Ok(())
                }
            }
            REValue::KeyValueStore { .. } => Ok(()),
            REValue::Component { .. } => Ok(()),
            REValue::Vault(..) => Ok(()),
            REValue::Resource(..) => Ok(()),
            REValue::NonFungibles(..) => Ok(()),
            REValue::Package(..) => Ok(()),
        }
    }

    pub fn verify_can_persist(&self) -> Result<(), RuntimeError> {
        match self {
            REValue::KeyValueStore { .. } => Ok(()),
            REValue::Component { .. } => Ok(()),
            REValue::Vault(..) => Ok(()),
            REValue::Resource(..) => Err(RuntimeError::ValueNotAllowed),
            REValue::NonFungibles(..) => Err(RuntimeError::ValueNotAllowed),
            REValue::Package(..) => Err(RuntimeError::ValueNotAllowed),
            REValue::Bucket(..) => Err(RuntimeError::ValueNotAllowed),
            REValue::Proof(..) => Err(RuntimeError::ValueNotAllowed),
        }
    }

    pub fn try_drop(self) -> Result<(), DropFailure> {
        match self {
            REValue::Package(..) => Err(DropFailure::Package),
            REValue::Vault(..) => Err(DropFailure::Vault),
            REValue::KeyValueStore { .. } => Err(DropFailure::KeyValueStore),
            REValue::Component { .. } => Err(DropFailure::Component),
            REValue::Bucket(..) => Err(DropFailure::Bucket),
            REValue::Resource(..) => Err(DropFailure::Resource),
            REValue::NonFungibles(..) => Err(DropFailure::Resource),
            REValue::Proof(proof) => {
                proof.drop();
                Ok(())
            }
        }
    }
}

impl Into<Bucket> for REValue {
    fn into(self) -> Bucket {
        match self {
            REValue::Bucket(bucket) => bucket,
            _ => panic!("Expected to be a bucket"),
        }
    }
}

impl Into<Proof> for REValue {
    fn into(self) -> Proof {
        match self {
            REValue::Proof(proof) => proof,
            _ => panic!("Expected to be a proof"),
        }
    }
}

impl Into<HashMap<NonFungibleId, NonFungible>> for REValue {
    fn into(self) -> HashMap<NonFungibleId, NonFungible> {
        match self {
            REValue::NonFungibles(non_fungibles) => non_fungibles,
            _ => panic!("Expected to be non fungibles"),
        }
    }
}

#[derive(Debug)]
pub enum REComplexValue {
    Component(Component),
}

impl REComplexValue {
    pub fn get_children(&self) -> Result<HashSet<ValueId>, RuntimeError> {
        match self {
            REComplexValue::Component(component) => {
                let value = ScryptoValue::from_slice(component.state())
                    .map_err(RuntimeError::DecodeError)?;
                Ok(value.value_ids())
            }
        }
    }

    pub fn into_re_value(self, children: HashMap<ValueId, REValue>) -> REValue {
        let mut pathed_values = HashMap::new();
        for (id, value) in children {
            pathed_values.insert(AddressPath::ValueId(id), value);
        }

        match self {
            REComplexValue::Component(component) => REValue::Component {
                component,
                child_values: InMemoryChildren::with_values(pathed_values),
            },
        }
    }
}

#[derive(Debug)]
pub enum REPrimitiveValue {
    Package(ValidatedPackage),
    Bucket(Bucket),
    Proof(Proof),
    KeyValue(PreCommittedKeyValueStore),
    Resource(ResourceManager),
    NonFungibles(ResourceAddress, HashMap<NonFungibleId, NonFungible>),
    Vault(Vault),
}

#[derive(Debug)]
pub enum REValueByComplexity {
    Primitive(REPrimitiveValue),
    Complex(REComplexValue),
}

impl Into<REValue> for REPrimitiveValue {
    fn into(self) -> REValue {
        match self {
            REPrimitiveValue::Resource(resource_manager) => REValue::Resource(resource_manager),
            REPrimitiveValue::NonFungibles(_resource_address, non_fungibles) => {
                REValue::NonFungibles(non_fungibles)
            }
            REPrimitiveValue::Package(package) => REValue::Package(package),
            REPrimitiveValue::Bucket(bucket) => REValue::Bucket(bucket),
            REPrimitiveValue::Proof(proof) => REValue::Proof(proof),
            REPrimitiveValue::KeyValue(store) => REValue::KeyValueStore(store),
            REPrimitiveValue::Vault(vault) => REValue::Vault(vault),
        }
    }
}

impl Into<REValueByComplexity> for ResourceManager {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Primitive(REPrimitiveValue::Resource(self))
    }
}

impl Into<REValueByComplexity> for (ResourceAddress, HashMap<NonFungibleId, NonFungible>) {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Primitive(REPrimitiveValue::NonFungibles(self.0, self.1))
    }
}

impl Into<REValueByComplexity> for Bucket {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Primitive(REPrimitiveValue::Bucket(self))
    }
}

impl Into<REValueByComplexity> for Proof {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Primitive(REPrimitiveValue::Proof(self))
    }
}

impl Into<REValueByComplexity> for Vault {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Primitive(REPrimitiveValue::Vault(self))
    }
}

impl Into<REValueByComplexity> for PreCommittedKeyValueStore {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Primitive(REPrimitiveValue::KeyValue(self))
    }
}

impl Into<REValueByComplexity> for ValidatedPackage {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Primitive(REPrimitiveValue::Package(self))
    }
}

impl Into<REValueByComplexity> for Component {
    fn into(self) -> REValueByComplexity {
        REValueByComplexity::Complex(REComplexValue::Component(self))
    }
}

#[derive(Debug)]
pub struct InMemoryChildren {
    child_values: HashMap<AddressPath, RefCell<REValue>>,
}

impl InMemoryChildren {
    pub fn new() -> Self {
        InMemoryChildren {
            child_values: HashMap::new(),
        }
    }

    pub fn with_values(values: HashMap<AddressPath, REValue>) -> Self {
        let mut child_values = HashMap::new();
        for (id, value) in values.into_iter() {
            child_values.insert(id, RefCell::new(value));
        }
        InMemoryChildren { child_values }
    }

    pub fn into_iter(self) -> IntoIter<AddressPath, RefCell<REValue>> {
        self.child_values.into_iter()
    }

    pub fn all_descendants(&self) -> Vec<AddressPath> {
        let mut descendents = Vec::new();
        for (id, value) in self.child_values.iter() {
            descendents.push(id.clone());
            let value = value.borrow();
            descendents.extend(value.all_descendants());
        }
        descendents
    }

    pub unsafe fn get_child(&self, path: &[AddressPath]) -> Ref<REValue> {
        let (first, rest) = path.split_first().unwrap();

        if rest.is_empty() {
            let value = self
                .child_values
                .get(first)
                .expect("Value expected to exist");
            return value.borrow();
        }

        let value = self.child_values.get(first).unwrap();
        let value = value.try_borrow_unguarded().unwrap();
        value.get_child(rest)
    }

    pub unsafe fn get_child_mut(&mut self, path: &[AddressPath]) -> RefMut<REValue> {
        let (first, rest) = path.split_first().unwrap();

        if rest.is_empty() {
            let value = self
                .child_values
                .get_mut(first)
                .expect("Value expected to exist");
            return value.borrow_mut();
        }

        let value = self.child_values.get_mut(first).unwrap();
        value.get_mut().get_child_mut(rest)
    }

    pub fn insert_children(&mut self, values: HashMap<AddressPath, REValue>) {
        for (id, value) in values {
            self.child_values.insert(id, RefCell::new(value));
        }
    }
}
