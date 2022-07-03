use sbor::rust::boxed::Box;
use sbor::rust::cell::Ref;
use sbor::rust::cell::{RefCell, RefMut};
use sbor::rust::collections::*;
use sbor::rust::format;
use sbor::rust::marker::*;
use sbor::rust::ops::Deref;
use sbor::rust::ops::DerefMut;
use sbor::rust::string::String;
use sbor::rust::string::ToString;
use sbor::rust::vec;
use sbor::rust::vec::Vec;
use sbor::*;
use scrypto::buffer::scrypto_decode;
use scrypto::core::{SNodeRef, ScryptoActor};
use scrypto::engine::types::*;
use scrypto::prelude::ComponentOffset;
use scrypto::resource::AuthZoneClearInput;
use scrypto::values::*;
use transaction::validation::*;

use crate::engine::*;
use crate::fee::*;
use crate::ledger::*;
use crate::model::*;
use crate::wasm::*;

/// A call frame is the basic unit that forms a transaction call stack, which keeps track of the
/// owned objects by this function.
pub struct CallFrame<
    'p, // parent lifetime
    't, // Track lifetime
    's, // Substate store lifetime
    'w, // WASM engine lifetime
    S,  // Substore store type
    W,  // WASM engine type
    I,  // WASM instance type
> where
    S: ReadableSubstateStore,
    W: WasmEngine<I>,
    I: WasmInstance,
{
    /// The transaction hash
    transaction_hash: Hash,
    /// The call depth
    depth: usize,
    /// Whether to show trace messages
    trace: bool,

    /// State track
    track: &'t mut Track<'s, S>,
    /// Wasm engine
    wasm_engine: &'w mut W,
    /// Wasm Instrumenter
    wasm_instrumenter: &'w mut WasmInstrumenter,

    /// All ref values accessible by this call frame. The value may be located in one of the following:
    /// 1. borrowed values
    /// 2. track
    value_refs: HashMap<ValueId, REValueInfo>,

    /// Owned Values
    owned_values: HashMap<ValueId, RefCell<REValue>>,
    worktop: Option<RefCell<Worktop>>,
    auth_zone: Option<RefCell<AuthZone>>,

    /// Borrowed Values from call frames up the stack
    frame_borrowed_values: HashMap<ValueId, RefMut<'p, REValue>>,
    caller_auth_zone: Option<&'p RefCell<AuthZone>>,

    /// There is a single cost unit counter and a single fee table per transaction execution.
    /// When a call ocurrs, they're passed from the parent to the child, and returned
    /// after the invocation.
    cost_unit_counter: Option<CostUnitCounter>,
    fee_table: Option<FeeTable>,

    phantom: PhantomData<I>,
}

#[macro_export]
macro_rules! trace {
    ( $self: expr, $level: expr, $msg: expr $( , $arg:expr )* ) => {
        #[cfg(not(feature = "alloc"))]
        if $self.trace {
            println!("{}[{:5}] {}", "  ".repeat($self.depth), $level, sbor::rust::format!($msg, $( $arg ),*));
        }
    };
}

fn verify_stored_value_update(
    old: &HashSet<ValueId>,
    missing: &HashSet<ValueId>,
) -> Result<(), RuntimeError> {
    // TODO: optimize intersection search
    for old_id in old.iter() {
        if !missing.contains(&old_id) {
            return Err(RuntimeError::StoredValueRemoved(old_id.clone()));
        }
    }

    for missing_id in missing.iter() {
        if !old.contains(missing_id) {
            return Err(RuntimeError::ValueNotFound(*missing_id));
        }
    }

    Ok(())
}

fn verify_stored_key(value: &ScryptoValue) -> Result<(), RuntimeError> {
    if !value.bucket_ids.is_empty() {
        return Err(RuntimeError::BucketNotAllowed);
    }
    if !value.proof_ids.is_empty() {
        return Err(RuntimeError::ProofNotAllowed);
    }
    if !value.vault_ids.is_empty() {
        return Err(RuntimeError::VaultNotAllowed);
    }
    if !value.kv_store_ids.is_empty() {
        return Err(RuntimeError::KeyValueStoreNotAllowed);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct REValueInfo {
    visible: bool,
    location: REValueLocation,
}

#[derive(Debug, Clone)]
pub enum REValueLocation {
    OwnedRoot(ValueId),
    Owned {
        root: ValueId,
        path: Vec<AddressPath>,
    },
    BorrowedRoot(ValueId),
    Borrowed {
        root: ValueId,
        path: Vec<AddressPath>,
    },
    Track(Address),
}

impl REValueLocation {
    fn child(&self, child_id: AddressPath) -> REValueLocation {
        match self {
            REValueLocation::OwnedRoot(root) => REValueLocation::Owned {
                root: root.clone(),
                path: vec![child_id],
            },
            REValueLocation::Owned {
                root,
                path: ancestors,
            } => {
                let mut next_path = ancestors.clone();
                next_path.push(child_id);
                REValueLocation::Owned {
                    root: root.clone(),
                    path: next_path,
                }
            }
            REValueLocation::BorrowedRoot(root) => REValueLocation::Borrowed {
                root: root.clone(),
                path: vec![child_id],
            },
            REValueLocation::Borrowed {
                root,
                path: ancestors,
            } => {
                let mut next_ancestors = ancestors.clone();
                next_ancestors.push(child_id);
                REValueLocation::Borrowed {
                    root: root.clone(),
                    path: next_ancestors,
                }
            }
            REValueLocation::Track(address) => REValueLocation::Track(address.child(child_id)),
        }
    }

    fn borrow_native_ref<'borrowed, S: ReadableSubstateStore>(
        &self,
        owned_values: &mut HashMap<ValueId, RefCell<REValue>>,
        borrowed_values: &mut HashMap<ValueId, RefMut<'borrowed, REValue>>,
        track: &mut Track<S>,
    ) -> RENativeValueRef<'borrowed> {
        match self {
            REValueLocation::BorrowedRoot(id) => {
                let owned = borrowed_values.remove(id).expect("Should exist");
                RENativeValueRef::OwnedRef(owned)
            }
            REValueLocation::Track(address) => {
                let value = track.take_value(address.clone());
                RENativeValueRef::Track(address.clone(), value)
            }
            REValueLocation::OwnedRoot(id) => {
                let cell = owned_values.remove(id).unwrap();
                let value = cell.into_inner();
                RENativeValueRef::Owned(value)
            }
            _ => panic!("Unexpected {:?}", self),
        }
    }

    fn to_owned_ref<'a, 'borrowed>(
        &self,
        owned_values: &'a HashMap<ValueId, RefCell<REValue>>,
        borrowed_values: &'a HashMap<ValueId, RefMut<'borrowed, REValue>>,
    ) -> Ref<'a, REValue> {
        match self {
            REValueLocation::OwnedRoot(root) => {
                let cell = owned_values.get(root).unwrap();
                cell.borrow()
            }
            REValueLocation::Owned { root, ref path } => unsafe {
                let root_value = owned_values
                    .get(&root)
                    .unwrap()
                    .try_borrow_unguarded()
                    .unwrap();
                root_value.get_child(path)
            },
            REValueLocation::Borrowed { root, path } => unsafe {
                let borrowed = borrowed_values.get(root).unwrap();
                borrowed.get_child(path)
            },
            _ => panic!("Not an owned ref"),
        }
    }

    fn to_ref<'a, 'p, 's, S: ReadableSubstateStore>(
        &self,
        owned_values: &'a HashMap<ValueId, RefCell<REValue>>,
        borrowed_values: &'a HashMap<ValueId, RefMut<'p, REValue>>,
        track: &'a Track<'s, S>,
    ) -> REValueRef<'a, 'p, 's, S> {
        match self {
            REValueLocation::OwnedRoot(_)
            | REValueLocation::Owned { .. }
            | REValueLocation::Borrowed { .. } => {
                REValueRef::Owned(self.to_owned_ref(owned_values, borrowed_values))
            }
            REValueLocation::BorrowedRoot(id) => {
                REValueRef::Borrowed(borrowed_values.get(id).unwrap())
            }
            REValueLocation::Track(address) => REValueRef::Track(track, address.clone()),
        }
    }

    fn to_owned_ref_mut<'a, 'borrowed>(
        &self,
        owned_values: &'a mut HashMap<ValueId, RefCell<REValue>>,
        borrowed_values: &'a mut HashMap<ValueId, RefMut<'borrowed, REValue>>,
    ) -> RefMut<'a, REValue> {
        match self {
            REValueLocation::OwnedRoot(id) => {
                let cell = owned_values.get_mut(id).unwrap();
                cell.borrow_mut()
            }
            REValueLocation::Owned { root, ref path } => unsafe {
                let root_value = owned_values.get_mut(&root).unwrap().get_mut();
                root_value.get_child_mut(path)
            },
            REValueLocation::Borrowed { root, path } => unsafe {
                let borrowed = borrowed_values.get_mut(root).unwrap();
                borrowed.get_child_mut(path)
            },
            _ => panic!("Not an owned ref"),
        }
    }

    fn to_ref_mut<'a, 'borrowed, 'c, 's, S: ReadableSubstateStore>(
        &self,
        owned_values: &'a mut HashMap<ValueId, RefCell<REValue>>,
        borrowed_values: &'a mut HashMap<ValueId, RefMut<'borrowed, REValue>>,
        track: &'c mut Track<'s, S>,
    ) -> REValueRefMut<'a, 'borrowed, 'c, 's, S> {
        match self {
            REValueLocation::OwnedRoot(_)
            | REValueLocation::Owned { .. }
            | REValueLocation::Borrowed { .. } => {
                REValueRefMut::Owned(self.to_owned_ref_mut(owned_values, borrowed_values))
            }
            REValueLocation::BorrowedRoot(id) => {
                REValueRefMut::Borrowed(borrowed_values.get_mut(id).unwrap())
            }
            REValueLocation::Track(address) => REValueRefMut::Track(track, address.clone()),
        }
    }
}

pub enum RENativeValueRef<'borrowed> {
    Owned(REValue),
    OwnedRef(RefMut<'borrowed, REValue>),
    Track(Address, SubstateValue),
}

impl<'borrowed> RENativeValueRef<'borrowed> {
    pub fn bucket(&mut self) -> &mut Bucket {
        match self {
            RENativeValueRef::OwnedRef(root) => match root.deref_mut() {
                REValue::Bucket(bucket) => bucket,
                _ => panic!("Expecting to be a bucket"),
            },
            _ => panic!("Expecting to be a bucket"),
        }
    }

    pub fn proof(&mut self) -> &mut Proof {
        match self {
            RENativeValueRef::OwnedRef(ref mut root) => match root.deref_mut() {
                REValue::Proof(proof) => proof,
                _ => panic!("Expecting to be a proof"),
            },
            _ => panic!("Expecting to be a proof"),
        }
    }

    pub fn vault(&mut self) -> &mut Vault {
        match self {
            RENativeValueRef::Owned(..) => panic!("Unexpected"),
            RENativeValueRef::OwnedRef(owned) => owned.vault_mut(),
            RENativeValueRef::Track(_address, value) => value.vault_mut(),
        }
    }

    pub fn component(&mut self) -> &mut Component {
        match self {
            RENativeValueRef::OwnedRef(owned) => owned.component_mut(),
            _ => panic!("Expecting to be a component"),
        }
    }

    pub fn package(&mut self) -> &ValidatedPackage {
        match self {
            RENativeValueRef::Track(_address, value) => value.package(),
            _ => panic!("Expecting to be tracked"),
        }
    }

    pub fn resource_manager(&mut self) -> &mut ResourceManager {
        match self {
            RENativeValueRef::Owned(owned) => owned.resource_manager_mut(),
            RENativeValueRef::Track(_address, value) => value.resource_manager_mut(),
            _ => panic!("Unexpected"),
        }
    }

    pub fn return_to_location<'a, S: ReadableSubstateStore>(
        self,
        value_id: ValueId,
        owned_values: &'a mut HashMap<ValueId, RefCell<REValue>>,
        borrowed_values: &mut HashMap<ValueId, RefMut<'borrowed, REValue>>,
        track: &mut Track<S>,
    ) {
        match self {
            RENativeValueRef::Owned(value) => {
                owned_values.insert(value_id, RefCell::new(value));
            }
            RENativeValueRef::OwnedRef(owned) => {
                borrowed_values.insert(value_id.clone(), owned);
            }
            RENativeValueRef::Track(address, value) => track.write_value(address, value),
        }
    }
}

pub enum REValueRef<'f, 'p, 's, S: ReadableSubstateStore> {
    Owned(Ref<'f, REValue>),
    Borrowed(&'f RefMut<'p, REValue>),
    Track(&'f Track<'s, S>, Address),
}

impl<'f, 'p, 's, S: ReadableSubstateStore> REValueRef<'f, 'p, 's, S> {
    pub fn vault(&self) -> &Vault {
        match self {
            REValueRef::Owned(owned) => owned.vault(),
            REValueRef::Track(track, address) => track.read_value(address.clone()).vault(),
            REValueRef::Borrowed(borrowed) => borrowed.vault(),
        }
    }

    pub fn resource_manager(&self) -> &ResourceManager {
        match self {
            REValueRef::Owned(owned) => owned.resource_manager(),
            REValueRef::Track(track, address) => {
                track.read_value(address.clone()).resource_manager()
            }
            REValueRef::Borrowed(borrowed) => borrowed.resource_manager(),
        }
    }

    pub fn component(&self) -> &Component {
        match self {
            REValueRef::Owned(owned) => owned.component(),
            REValueRef::Track(track, address) => track.read_value(address.clone()).component(),
            REValueRef::Borrowed(borrowed) => borrowed.component(),
        }
    }

    pub fn package(&self) -> &ValidatedPackage {
        match self {
            REValueRef::Owned(owned) => owned.package(),
            REValueRef::Track(track, address) => track.read_value(address.clone()).package(),
            _ => panic!("Unexpected component ref"),
        }
    }
}

pub enum REValueRefMut<'a, 'b, 'c, 's, S: ReadableSubstateStore> {
    Owned(RefMut<'a, REValue>),
    Borrowed(&'a mut RefMut<'b, REValue>),
    Track(&'c mut Track<'s, S>, Address),
}

impl<'a, 'b, 'c, 's, S: ReadableSubstateStore> REValueRefMut<'a, 'b, 'c, 's, S> {
    fn kv_store_put(
        &mut self,
        key: Vec<u8>,
        value: ScryptoValue,
        to_store: HashMap<AddressPath, REValue>,
    ) {
        match self {
            REValueRefMut::Owned(owned) => {
                owned.kv_store_mut().put(key, value, to_store);
            }
            REValueRefMut::Borrowed(..) => {
                panic!("Not supported");
            }
            REValueRefMut::Track(track, address) => {
                track.set_key_value(
                    address.clone(),
                    key.clone(),
                    SubstateValue::KeyValueStoreEntry(Some(value.raw)),
                );

                let entry_address = address.child(AddressPath::Key(key));
                track.insert_objects(to_store, entry_address);
            }
        }
    }

    fn kv_store_get(&mut self, key: &[u8]) -> ScryptoValue {
        let maybe_value = match self {
            REValueRefMut::Owned(owned) => {
                let store = owned.kv_store_mut();
                store.get(key).map(|(v, _children)| v.dom.clone())
            }
            REValueRefMut::Borrowed(..) => {
                panic!("Not supported");
            }
            REValueRefMut::Track(track, address) => {
                let substate_value = track.read_key_value(address.clone(), key.to_vec());
                substate_value
                    .kv_entry()
                    .as_ref()
                    .map(|bytes| decode_any(bytes).unwrap())
            }
        };

        // TODO: Cleanup
        let value = maybe_value.map_or(
            Value::Option {
                value: Box::new(Option::None),
            },
            |v| Value::Option {
                value: Box::new(Some(v)),
            },
        );
        ScryptoValue::from_value(value).unwrap()
    }

    fn non_fungible_get(&mut self, id: &NonFungibleId) -> ScryptoValue {
        match self {
            REValueRefMut::Owned(owned) => {
                ScryptoValue::from_typed(&owned.non_fungibles().get(id).cloned())
            }
            REValueRefMut::Borrowed(..) => {
                panic!("Not supported");
            }
            REValueRefMut::Track(track, address) => {
                let value = track.read_key_value(address.clone(), id.to_vec());
                ScryptoValue::from_typed(value.non_fungible())
            }
        }
    }

    fn non_fungible_remove(&mut self, id: &NonFungibleId) {
        match self {
            REValueRefMut::Owned(..) => {
                panic!("Not supported");
            }
            REValueRefMut::Borrowed(..) => {
                panic!("Not supported");
            }
            REValueRefMut::Track(track, address) => {
                track.set_key_value(
                    address.clone(),
                    id.to_vec(),
                    SubstateValue::NonFungible(None),
                );
            }
        }
    }

    fn non_fungible_put(&mut self, id: NonFungibleId, value: ScryptoValue) {
        match self {
            REValueRefMut::Owned(owned) => {
                let non_fungible: NonFungible =
                    scrypto_decode(&value.raw).expect("Should not fail.");
                owned.non_fungibles_mut().insert(id, non_fungible);
            }
            REValueRefMut::Borrowed(..) => {
                panic!("Not supported");
            }
            REValueRefMut::Track(track, address) => {
                let non_fungible: NonFungible =
                    scrypto_decode(&value.raw).expect("Should not fail.");
                track.set_key_value(
                    address.clone(),
                    id.to_vec(),
                    SubstateValue::NonFungible(Some(non_fungible)),
                );
            }
        }
    }

    fn component_put(&mut self, value: ScryptoValue, to_store: HashMap<AddressPath, REValue>) {
        match self {
            REValueRefMut::Track(track, address) => {
                track.write_component_value(address.clone(), value.raw);
                track.insert_objects(to_store, address.clone());
            }
            REValueRefMut::Borrowed(owned) => unsafe {
                let component = owned.component_mut();
                component.set_state(value.raw);
                owned.insert_children(to_store);
            },
            _ => panic!("Unexpected component ref"),
        }
    }

    fn component(&mut self) -> &Component {
        match self {
            REValueRefMut::Owned(owned) => owned.component(),
            REValueRefMut::Borrowed(borrowed) => borrowed.component(),
            REValueRefMut::Track(track, address) => {
                let component_val = track.read_value(address.clone());
                component_val.component()
            }
        }
    }
}

pub enum StaticSNodeState {
    Package,
    Resource,
    System,
    TransactionProcessor,
}

pub enum SNodeExecution<'a> {
    Static(StaticSNodeState),
    Consumed(ValueId),
    AuthZone(RefMut<'a, AuthZone>),
    Worktop(RefMut<'a, Worktop>),
    ValueRef(ValueId),
    Scrypto(ScryptoActorInfo, PackageAddress),
}

pub enum SubstateAddress {
    KeyValueEntry(KeyValueStoreId, ScryptoValue),
    NonFungible(ResourceAddress, NonFungibleId),
    Component(ComponentAddress, ComponentOffset),
}

impl<'p, 't, 's, 'w, S, W, I> CallFrame<'p, 't, 's, 'w, S, W, I>
where
    S: ReadableSubstateStore,
    W: WasmEngine<I>,
    I: WasmInstance,
{
    pub fn new_root(
        verbose: bool,
        transaction_hash: Hash,
        signer_public_keys: Vec<EcdsaPublicKey>,
        track: &'t mut Track<'s, S>,
        wasm_engine: &'w mut W,
        wasm_instrumenter: &'w mut WasmInstrumenter,
        cost_unit_counter: CostUnitCounter,
        fee_table: FeeTable,
    ) -> Self {
        let signer_non_fungible_ids: BTreeSet<NonFungibleId> = signer_public_keys
            .clone()
            .into_iter()
            .map(|public_key| NonFungibleId::from_bytes(public_key.to_vec()))
            .collect();

        let mut initial_auth_zone_proofs = Vec::new();
        if !signer_non_fungible_ids.is_empty() {
            // Proofs can't be zero amount
            let mut ecdsa_bucket = Bucket::new(ResourceContainer::new_non_fungible(
                ECDSA_TOKEN,
                signer_non_fungible_ids,
            ));
            let ecdsa_proof = ecdsa_bucket.create_proof(ECDSA_TOKEN_BUCKET_ID).unwrap();
            initial_auth_zone_proofs.push(ecdsa_proof);
        }

        Self::new(
            transaction_hash,
            0,
            verbose,
            track,
            wasm_engine,
            wasm_instrumenter,
            Some(RefCell::new(AuthZone::new_with_proofs(
                initial_auth_zone_proofs,
            ))),
            Some(RefCell::new(Worktop::new())),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            None,
            cost_unit_counter,
            fee_table,
        )
    }

    pub fn new(
        transaction_hash: Hash,
        depth: usize,
        trace: bool,
        track: &'t mut Track<'s, S>,
        wasm_engine: &'w mut W,
        wasm_instrumenter: &'w mut WasmInstrumenter,
        auth_zone: Option<RefCell<AuthZone>>,
        worktop: Option<RefCell<Worktop>>,
        owned_values: HashMap<ValueId, RefCell<REValue>>,
        value_refs: HashMap<ValueId, REValueInfo>,
        frame_borrowed_values: HashMap<ValueId, RefMut<'p, REValue>>,
        caller_auth_zone: Option<&'p RefCell<AuthZone>>,
        cost_unit_counter: CostUnitCounter,
        fee_table: FeeTable,
    ) -> Self {
        Self {
            transaction_hash,
            depth,
            trace,
            track,
            wasm_engine,
            wasm_instrumenter,
            owned_values,
            value_refs,
            frame_borrowed_values,
            worktop,
            auth_zone,
            caller_auth_zone,
            cost_unit_counter: Some(cost_unit_counter),
            fee_table: Some(fee_table),
            phantom: PhantomData,
        }
    }

    fn drop_owned_values(&mut self) -> Result<(), RuntimeError> {
        for (_, value) in self.owned_values.drain() {
            trace!(self, Level::Warn, "Dangling value: {:?}", value);
            value
                .into_inner()
                .try_drop()
                .map_err(|e| RuntimeError::DropFailure(e))?;
        }

        if let Some(ref_worktop) = &self.worktop {
            let worktop = ref_worktop.borrow();
            if !worktop.is_empty() {
                trace!(self, Level::Warn, "Resource worktop is not empty");
                return Err(RuntimeError::DropFailure(DropFailure::Worktop));
            }
        }

        Ok(())
    }

    fn process_call_data(validated: &ScryptoValue) -> Result<(), RuntimeError> {
        if !validated.kv_store_ids.is_empty() {
            return Err(RuntimeError::KeyValueStoreNotAllowed);
        }
        if !validated.vault_ids.is_empty() {
            return Err(RuntimeError::VaultNotAllowed);
        }
        Ok(())
    }

    fn process_return_data(
        &mut self,
        from: Option<SNodeRef>,
        validated: &ScryptoValue,
    ) -> Result<(), RuntimeError> {
        if !validated.kv_store_ids.is_empty() {
            return Err(RuntimeError::KeyValueStoreNotAllowed);
        }

        // Allow vaults to be returned from ResourceStatic
        // TODO: Should we allow vaults to be returned by any component?
        if !matches!(from, Some(SNodeRef::ResourceRef(_))) {
            if !validated.vault_ids.is_empty() {
                return Err(RuntimeError::VaultNotAllowed);
            }
        }

        Ok(())
    }

    pub fn run(
        &mut self,
        snode_ref: Option<SNodeRef>, // TODO: Remove, abstractions between invoke_snode() and run() are a bit messy right now
        execution: SNodeExecution<'p>,
        fn_ident: &str,
        input: ScryptoValue,
    ) -> Result<(ScryptoValue, HashMap<ValueId, REValue>), RuntimeError> {
        trace!(
            self,
            Level::Debug,
            "Run started! Remainging cost units: {}",
            self.cost_unit_counter().remaining()
        );

        Self::cost_unit_counter_helper(&mut self.cost_unit_counter)
            .consume(Self::fee_table_helper(&mut self.fee_table).engine_run_cost())
            .map_err(RuntimeError::CostingError)?;

        let output = {
            let rtn = match execution {
                SNodeExecution::Static(state) => match state {
                    StaticSNodeState::System => System::static_main(fn_ident, input, self)
                        .map_err(RuntimeError::SystemError),
                    StaticSNodeState::TransactionProcessor => TransactionProcessor::static_main(
                        fn_ident, input, self,
                    )
                    .map_err(|e| match e {
                        TransactionProcessorError::InvalidRequestData(_) => panic!("Illegal state"),
                        TransactionProcessorError::InvalidMethod => panic!("Illegal state"),
                        TransactionProcessorError::RuntimeError(e) => e,
                    }),
                    StaticSNodeState::Package => {
                        ValidatedPackage::static_main(fn_ident, input, self)
                            .map_err(RuntimeError::PackageError)
                    }
                    StaticSNodeState::Resource => {
                        ResourceManager::static_main(fn_ident, input, self)
                            .map_err(RuntimeError::ResourceManagerError)
                    }
                },
                SNodeExecution::Consumed(value_id) => match value_id {
                    ValueId::Bucket(..) => Bucket::consuming_main(value_id, fn_ident, input, self)
                        .map_err(RuntimeError::BucketError),
                    ValueId::Proof(..) => Proof::main_consume(value_id, fn_ident, input, self)
                        .map_err(RuntimeError::ProofError),
                    ValueId::Component(..) => {
                        Component::main_consume(value_id, fn_ident, input, self)
                            .map_err(RuntimeError::ComponentError)
                    }
                    _ => panic!("Unexpected"),
                },
                SNodeExecution::AuthZone(mut auth_zone) => auth_zone
                    .main(fn_ident, input, self)
                    .map_err(RuntimeError::AuthZoneError),
                SNodeExecution::Worktop(mut worktop) => worktop
                    .main(fn_ident, input, self)
                    .map_err(RuntimeError::WorktopError),
                SNodeExecution::ValueRef(value_id) => match value_id {
                    ValueId::Bucket(bucket_id) => Bucket::main(bucket_id, fn_ident, input, self)
                        .map_err(RuntimeError::BucketError),
                    ValueId::Proof(..) => Proof::main(value_id, fn_ident, input, self)
                        .map_err(RuntimeError::ProofError),
                    ValueId::Vault(vault_id) => Vault::main(vault_id, fn_ident, input, self)
                        .map_err(RuntimeError::VaultError),
                    ValueId::Component(..) => Component::main(value_id, fn_ident, input, self)
                        .map_err(RuntimeError::ComponentError),
                    ValueId::Resource(resource_address) => {
                        ResourceManager::main(resource_address, fn_ident, input, self)
                            .map_err(RuntimeError::ResourceManagerError)
                    }
                    _ => panic!("Unexpected"),
                },
                SNodeExecution::Scrypto(ref actor, package_address) => {
                    let output = {
                        let package = self.track.read_value(package_address).package();
                        let wasm_metering_params =
                            Self::fee_table_helper(&self.fee_table).wasm_metering_params();
                        let instrumented_code = self
                            .wasm_instrumenter
                            .instrument(package.code(), &wasm_metering_params);
                        let mut instance = self.wasm_engine.instantiate(instrumented_code);
                        let blueprint_abi = package
                            .blueprint_abi(actor.blueprint_name())
                            .expect("Blueprint should exist");
                        let export_name = &blueprint_abi
                            .get_fn_abi(fn_ident)
                            .unwrap()
                            .export_name
                            .to_string();
                        let mut runtime: Box<dyn WasmRuntime> =
                            Box::new(RadixEngineWasmRuntime::new(actor.clone(), self));
                        instance
                            .invoke_export(&export_name, &input, &mut runtime)
                            .map_err(|e| match e {
                                // Flatten error code for more readable transaction receipt
                                InvokeError::RuntimeError(e) => e,
                                e @ _ => RuntimeError::InvokeError(e.into()),
                            })?
                    };

                    let package = self.track.read_value(package_address).package();
                    let blueprint_abi = package
                        .blueprint_abi(actor.blueprint_name())
                        .expect("Blueprint should exist");
                    let fn_abi = blueprint_abi.get_fn_abi(fn_ident).unwrap();
                    if !fn_abi.output.matches(&output.dom) {
                        Err(RuntimeError::InvalidFnOutput {
                            fn_ident: fn_ident.to_string(),
                            output: output.dom,
                        })
                    } else {
                        Ok(output)
                    }
                }
            }?;

            rtn
        };

        // Prevent vaults/kvstores from being returned
        self.process_return_data(snode_ref, &output)?;

        // Take values to return
        let values_to_take = output.value_ids();
        let (taken_values, mut missing) = self.take_available_values(values_to_take, false)?;
        let first_missing_value = missing.drain().nth(0);
        if let Some(missing_value) = first_missing_value {
            return Err(RuntimeError::ValueNotFound(missing_value));
        }

        // drop proofs and check resource leak
        if self.auth_zone.is_some() {
            self.invoke_snode(
                SNodeRef::AuthZoneRef,
                "clear".to_string(),
                ScryptoValue::from_typed(&AuthZoneClearInput {}),
            )?;
        }
        self.drop_owned_values()?;

        trace!(
            self,
            Level::Debug,
            "Run finished! Remainging cost units: {}",
            self.cost_unit_counter().remaining()
        );

        Ok((output, taken_values))
    }

    fn cost_unit_counter_helper(counter: &mut Option<CostUnitCounter>) -> &mut CostUnitCounter {
        counter
            .as_mut()
            .expect("Frame doens't own a cost unit counter")
    }

    pub fn cost_unit_counter(&mut self) -> &mut CostUnitCounter {
        // Use helper method to support paritial borrow of self
        // See https://users.rust-lang.org/t/how-to-partially-borrow-from-struct/32221
        Self::cost_unit_counter_helper(&mut self.cost_unit_counter)
    }

    fn fee_table_helper(fee_table: &Option<FeeTable>) -> &FeeTable {
        fee_table.as_ref().expect("Frame doens't own a fee table")
    }

    pub fn fee_table(&self) -> &FeeTable {
        // Use helper method to support paritial borrow of self
        // See https://users.rust-lang.org/t/how-to-partially-borrow-from-struct/32221
        Self::fee_table_helper(&self.fee_table)
    }

    fn take_available_values(
        &mut self,
        value_ids: HashSet<ValueId>,
        persist_only: bool,
    ) -> Result<(HashMap<ValueId, REValue>, HashSet<ValueId>), RuntimeError> {
        let (taken, missing) = {
            let mut taken_values = HashMap::new();
            let mut missing_values = HashSet::new();

            for id in value_ids {
                let maybe = self.owned_values.remove(&id);
                if let Some(celled_value) = maybe {
                    let value = celled_value.into_inner();
                    value.verify_can_move()?;
                    if persist_only {
                        value.verify_can_persist()?;
                    }
                    taken_values.insert(id, value);
                } else {
                    missing_values.insert(id);
                }
            }

            (taken_values, missing_values)
        };

        // Moved values must have their references removed
        for (id, value) in &taken {
            self.value_refs.remove(id);
            for id in value.all_descendants() {
                match id {
                    AddressPath::ValueId(value_id) => {
                        self.value_refs.remove(&value_id);
                    }
                    AddressPath::Key(..) => {}
                }
            }
        }

        Ok((taken, missing))
    }

    fn read_value_internal(
        &mut self,
        address: &SubstateAddress,
    ) -> Result<(REValueLocation, ScryptoValue), RuntimeError> {
        let value_id = match address {
            SubstateAddress::Component(component_address, ..) => {
                ValueId::Component(*component_address)
            }
            SubstateAddress::NonFungible(resource_address, ..) => {
                ValueId::NonFungibles(*resource_address)
            }
            SubstateAddress::KeyValueEntry(kv_store_id, ..) => ValueId::KeyValueStore(*kv_store_id),
        };

        // Get location
        // Note this must be run AFTER values are taken, otherwise there would be inconsistent readable_values state
        let (value_info, address_borrowed) = self
            .value_refs
            .get(&value_id)
            .cloned()
            .map(|v| (v, None))
            .or_else(|| {
                // Allow global read access to any component info
                if let SubstateAddress::Component(component_address, ComponentOffset::Info) =
                    address
                {
                    if self.owned_values.contains_key(&value_id) {
                        return Some((
                            REValueInfo {
                                location: REValueLocation::OwnedRoot(value_id.clone()),
                                visible: true,
                            },
                            None,
                        ));
                    } else if self.track.take_lock(*component_address, false).is_ok() {
                        return Some((
                            REValueInfo {
                                location: REValueLocation::Track(Address::GlobalComponent(
                                    *component_address,
                                )),
                                visible: true,
                            },
                            Some(component_address),
                        ));
                    }
                }

                None
            })
            .ok_or_else(|| RuntimeError::InvalidDataAccess(value_id))?;
        if !value_info.visible {
            return Err(RuntimeError::InvalidDataAccess(value_id));
        }
        let location = &value_info.location;

        // Read current value
        let current_value = {
            let mut value_ref = location.to_ref_mut(
                &mut self.owned_values,
                &mut self.frame_borrowed_values,
                &mut self.track,
            );
            match &address {
                SubstateAddress::Component(.., offset) => match offset {
                    ComponentOffset::State => {
                        ScryptoValue::from_slice(value_ref.component().state())
                            .expect("Expected to decode")
                    }
                    ComponentOffset::Info => {
                        ScryptoValue::from_typed(&value_ref.component().info())
                    }
                },
                SubstateAddress::KeyValueEntry(.., key) => {
                    verify_stored_key(key)?;
                    value_ref.kv_store_get(&key.raw)
                }
                SubstateAddress::NonFungible(.., id) => value_ref.non_fungible_get(id),
            }
        };

        // TODO: Remove, currently a hack to allow for global component info retrieval
        if let Some(component_address) = address_borrowed {
            self.track.release_lock(*component_address);
        }

        Ok((location.clone(), current_value))
    }
}

impl<'p, 't, 's, 'w, S, W, I> SystemApi<'p, 's, W, I, S> for CallFrame<'p, 't, 's, 'w, S, W, I>
where
    S: ReadableSubstateStore,
    W: WasmEngine<I>,
    I: WasmInstance,
{
    fn invoke_snode(
        &mut self,
        snode_ref: SNodeRef,
        fn_ident: String,
        input: ScryptoValue,
    ) -> Result<ScryptoValue, RuntimeError> {
        trace!(
            self,
            Level::Debug,
            "Invoking: {:?} {:?}",
            snode_ref,
            &fn_ident
        );

        // Prevent vaults/kvstores from being moved
        Self::process_call_data(&input)?;

        // Figure out what buckets and proofs to move from this process
        let values_to_take = input.value_ids();
        let (taken_values, mut missing) = self.take_available_values(values_to_take, false)?;
        let first_missing_value = missing.drain().nth(0);
        if let Some(missing_value) = first_missing_value {
            return Err(RuntimeError::ValueNotFound(missing_value));
        }

        let mut next_owned_values = HashMap::new();

        // Internal state update to taken values
        for (id, mut value) in taken_values {
            trace!(self, Level::Debug, "Sending value: {:?}", value);
            match &mut value {
                REValue::Proof(proof) => proof.change_to_restricted(),
                _ => {}
            }
            next_owned_values.insert(id, RefCell::new(value));
        }

        let mut locked_values = HashSet::new();
        let mut value_refs = HashMap::new();
        let mut next_borrowed_values = HashMap::new();

        // Authorization and state load
        let (loaded_snode, method_auths) = match &snode_ref {
            SNodeRef::TransactionProcessor => {
                // FIXME: only TransactionExecutor can invoke this function
                Ok((
                    SNodeExecution::Static(StaticSNodeState::TransactionProcessor),
                    vec![],
                ))
            }
            SNodeRef::PackageStatic => {
                Ok((SNodeExecution::Static(StaticSNodeState::Package), vec![]))
            }
            SNodeRef::SystemStatic => {
                Ok((SNodeExecution::Static(StaticSNodeState::System), vec![]))
            }
            SNodeRef::ResourceStatic => {
                Ok((SNodeExecution::Static(StaticSNodeState::Resource), vec![]))
            }
            SNodeRef::Consumed(value_id) => {
                let value = self
                    .owned_values
                    .remove(value_id)
                    .ok_or(RuntimeError::ValueNotFound(*value_id))?
                    .into_inner();

                let method_auths = match &value {
                    REValue::Bucket(bucket) => {
                        let resource_address = bucket.resource_address();
                        self.track
                            .take_lock(resource_address, true)
                            .expect("Should not fail.");
                        locked_values.insert(resource_address.clone().into());
                        let resource_manager =
                            self.track.read_value(resource_address).resource_manager();
                        let method_auth = resource_manager.get_consuming_bucket_auth(&fn_ident);
                        value_refs.insert(
                            ValueId::Resource(resource_address),
                            REValueInfo {
                                location: REValueLocation::Track(Address::Resource(
                                    resource_address,
                                )),
                                visible: true,
                            },
                        );
                        value_refs.insert(
                            ValueId::NonFungibles(resource_address),
                            REValueInfo {
                                location: REValueLocation::Track(Address::NonFungibleSet(
                                    resource_address,
                                )),
                                visible: true,
                            },
                        );
                        vec![method_auth.clone()]
                    }
                    REValue::Proof(_) => vec![],
                    REValue::Component { component, .. } => {
                        let package_address = component.package_address();
                        self.track
                            .take_lock(package_address, false)
                            .expect("Should not fail.");
                        locked_values.insert(package_address.clone().into());
                        value_refs.insert(
                            ValueId::Package(package_address),
                            REValueInfo {
                                location: REValueLocation::Track(Address::Package(package_address)),
                                visible: true,
                            },
                        );
                        vec![]
                    }
                    _ => return Err(RuntimeError::MethodDoesNotExist(fn_ident.clone())),
                };

                next_owned_values.insert(*value_id, RefCell::new(value));

                Ok((SNodeExecution::Consumed(*value_id), method_auths))
            }
            SNodeRef::AuthZoneRef => {
                if let Some(auth_zone) = &self.auth_zone {
                    for resource_address in &input.resource_addresses {
                        self.track
                            .take_lock(resource_address.clone(), false)
                            .map_err(|e| match e {
                                TrackError::NotFound => {
                                    RuntimeError::ResourceManagerNotFound(resource_address.clone())
                                }
                                TrackError::Reentrancy => {
                                    panic!("Package reentrancy error should never occur.")
                                }
                            })?;
                        locked_values.insert(resource_address.clone().into());
                        value_refs.insert(
                            ValueId::Resource(resource_address.clone()),
                            REValueInfo {
                                location: REValueLocation::Track(Address::Resource(
                                    resource_address.clone(),
                                )),
                                visible: true,
                            },
                        );
                    }
                    let borrowed = auth_zone.borrow_mut();
                    Ok((SNodeExecution::AuthZone(borrowed), vec![]))
                } else {
                    Err(RuntimeError::AuthZoneDoesNotExist)
                }
            }
            SNodeRef::WorktopRef => {
                if let Some(worktop_ref) = &self.worktop {
                    for resource_address in &input.resource_addresses {
                        self.track
                            .take_lock(resource_address.clone(), false)
                            .map_err(|e| match e {
                                TrackError::NotFound => {
                                    RuntimeError::ResourceManagerNotFound(resource_address.clone())
                                }
                                TrackError::Reentrancy => {
                                    panic!("Package reentrancy error should never occur.")
                                }
                            })?;
                        locked_values.insert(resource_address.clone().into());
                        value_refs.insert(
                            ValueId::Resource(resource_address.clone()),
                            REValueInfo {
                                location: REValueLocation::Track(Address::Resource(
                                    resource_address.clone(),
                                )),
                                visible: true,
                            },
                        );
                    }
                    let worktop = worktop_ref.borrow_mut();
                    Ok((SNodeExecution::Worktop(worktop), vec![]))
                } else {
                    Err(RuntimeError::WorktopDoesNotExist)
                }
            }
            SNodeRef::ResourceRef(resource_address) => {
                let value_id = ValueId::Resource(*resource_address);
                let address: Address = Address::Resource(*resource_address);
                self.track
                    .take_lock(address.clone(), true)
                    .map_err(|e| match e {
                        TrackError::NotFound => {
                            RuntimeError::ResourceManagerNotFound(resource_address.clone())
                        }
                        TrackError::Reentrancy => {
                            panic!("Resource call has caused reentrancy")
                        }
                    })?;
                locked_values.insert(address.clone());
                let resource_manager = self.track.read_value(address).resource_manager();
                let method_auth = resource_manager.get_auth(&fn_ident, &input).clone();
                value_refs.insert(
                    value_id.clone(),
                    REValueInfo {
                        location: REValueLocation::Track(Address::Resource(*resource_address)),
                        visible: true,
                    },
                );
                value_refs.insert(
                    ValueId::NonFungibles(*resource_address),
                    REValueInfo {
                        location: REValueLocation::Track(Address::NonFungibleSet(
                            *resource_address,
                        )),
                        visible: true,
                    },
                );

                Ok((SNodeExecution::ValueRef(value_id), vec![method_auth]))
            }
            SNodeRef::BucketRef(bucket_id) => {
                let value_id = ValueId::Bucket(*bucket_id);
                let bucket_cell = self
                    .owned_values
                    .get(&value_id)
                    .ok_or(RuntimeError::BucketNotFound(bucket_id.clone()))?;
                let ref_mut = bucket_cell.borrow_mut();
                next_borrowed_values.insert(value_id.clone(), ref_mut);
                value_refs.insert(
                    value_id.clone(),
                    REValueInfo {
                        location: REValueLocation::BorrowedRoot(value_id.clone()),
                        visible: true,
                    },
                );

                Ok((SNodeExecution::ValueRef(value_id), vec![]))
            }
            SNodeRef::ProofRef(proof_id) => {
                let value_id = ValueId::Proof(*proof_id);
                let proof_cell = self
                    .owned_values
                    .get(&value_id)
                    .ok_or(RuntimeError::ProofNotFound(proof_id.clone()))?;
                let ref_mut = proof_cell.borrow_mut();
                next_borrowed_values.insert(value_id.clone(), ref_mut);
                value_refs.insert(
                    value_id.clone(),
                    REValueInfo {
                        location: REValueLocation::BorrowedRoot(value_id.clone()),
                        visible: true,
                    },
                );
                Ok((SNodeExecution::ValueRef(value_id), vec![]))
            }
            SNodeRef::Scrypto(actor) => match actor {
                ScryptoActor::Blueprint(package_address, blueprint_name) => {
                    self.track
                        .take_lock(package_address.clone(), false)
                        .map_err(|e| match e {
                            TrackError::NotFound => RuntimeError::PackageNotFound(*package_address),
                            TrackError::Reentrancy => {
                                panic!("Package reentrancy error should never occur.")
                            }
                        })?;
                    locked_values.insert(package_address.clone().into());
                    let package = self.track.read_value(package_address.clone()).package();
                    let abi = package.blueprint_abi(blueprint_name).ok_or(
                        RuntimeError::BlueprintNotFound(
                            package_address.clone(),
                            blueprint_name.clone(),
                        ),
                    )?;
                    let fn_abi = abi
                        .get_fn_abi(&fn_ident)
                        .ok_or(RuntimeError::MethodDoesNotExist(fn_ident.clone()))?;
                    if !fn_abi.input.matches(&input.dom) {
                        return Err(RuntimeError::InvalidFnInput {
                            fn_ident,
                            input: input.dom,
                        });
                    }
                    Ok((
                        SNodeExecution::Scrypto(
                            ScryptoActorInfo::blueprint(
                                package_address.clone(),
                                blueprint_name.clone(),
                            ),
                            package_address.clone(),
                        ),
                        vec![],
                    ))
                }
                ScryptoActor::Component(component_address) => {
                    let component_address = *component_address;

                    // Find value
                    let value_id = ValueId::Component(component_address);
                    let cur_location = if self.owned_values.contains_key(&value_id) {
                        REValueLocation::OwnedRoot(value_id.clone())
                    } else if let Some(REValueInfo { location, .. }) =
                        self.value_refs.get(&value_id)
                    {
                        location.clone()
                    } else {
                        REValueLocation::Track(Address::GlobalComponent(component_address))
                    };

                    // Lock values and setup next frame
                    let next_frame_location = match cur_location {
                        REValueLocation::Track(address) => {
                            self.track
                                .take_lock(address.clone(), true)
                                .map_err(|e| match e {
                                    TrackError::NotFound => {
                                        RuntimeError::ComponentNotFound(component_address)
                                    }
                                    TrackError::Reentrancy => {
                                        RuntimeError::ComponentReentrancy(component_address)
                                    }
                                })?;
                            locked_values.insert(address.clone());
                            REValueLocation::Track(address)
                        }
                        REValueLocation::OwnedRoot(_) | REValueLocation::Borrowed { .. } => {
                            let owned_ref = cur_location.to_owned_ref_mut(
                                &mut self.owned_values,
                                &mut self.frame_borrowed_values,
                            );
                            next_borrowed_values.insert(value_id.clone(), owned_ref);
                            REValueLocation::BorrowedRoot(value_id.clone())
                        }
                        _ => panic!("Unexpected"),
                    };

                    let actor_info = {
                        let value_ref = next_frame_location.to_ref(
                            &mut next_owned_values,
                            &mut next_borrowed_values,
                            &mut self.track,
                        );
                        let component = value_ref.component();
                        ScryptoActorInfo::component(
                            component.package_address(),
                            component.blueprint_name().to_string(),
                            component_address,
                        )
                    };

                    // Retrieve Method Authorization
                    let (method_auths, package_address) = {
                        let package_address = actor_info.package_address().clone();
                        let blueprint_name = actor_info.blueprint_name().to_string();
                        self.track
                            .take_lock(package_address, false)
                            .expect("Should never fail");
                        locked_values.insert(package_address.clone().into());
                        let package = self.track.read_value(package_address).package();
                        let abi = package
                            .blueprint_abi(&blueprint_name)
                            .expect("Blueprint not found for existing component");
                        let fn_abi = abi
                            .get_fn_abi(&fn_ident)
                            .ok_or(RuntimeError::MethodDoesNotExist(fn_ident.clone()))?;
                        if !fn_abi.input.matches(&input.dom) {
                            return Err(RuntimeError::InvalidFnInput {
                                fn_ident,
                                input: input.dom,
                            });
                        }

                        let method_auths = {
                            let value_ref = next_frame_location.to_ref(
                                &next_owned_values,
                                &next_borrowed_values,
                                &self.track,
                            );
                            value_ref
                                .component()
                                .method_authorization(&abi.structure, &fn_ident)
                        };

                        (method_auths, package_address)
                    };

                    value_refs.insert(
                        value_id,
                        REValueInfo {
                            location: next_frame_location,
                            visible: true,
                        },
                    );

                    Ok((
                        SNodeExecution::Scrypto(actor_info, package_address),
                        method_auths,
                    ))
                }
            },
            SNodeRef::Component(component_address) => {
                let component_address = *component_address;

                // Find value
                let value_id = ValueId::Component(component_address);
                let cur_location = if self.owned_values.contains_key(&value_id) {
                    REValueLocation::OwnedRoot(value_id.clone())
                } else {
                    return Err(RuntimeError::NotSupported);
                };

                // Setup next frame
                match cur_location {
                    REValueLocation::OwnedRoot(_) => {
                        let owned_ref = cur_location.to_owned_ref_mut(
                            &mut self.owned_values,
                            &mut self.frame_borrowed_values,
                        );

                        // Lock package
                        let package_address = owned_ref.component().package_address();
                        self.track
                            .take_lock(package_address, false)
                            .map_err(|e| match e {
                                TrackError::NotFound => panic!("Should exist"),
                                TrackError::Reentrancy => RuntimeError::PackageReentrancy,
                            })?;
                        locked_values.insert(package_address.into());
                        value_refs.insert(
                            ValueId::Package(package_address),
                            REValueInfo {
                                location: REValueLocation::Track(Address::Package(package_address)),
                                visible: true,
                            },
                        );

                        next_borrowed_values.insert(value_id, owned_ref);
                        value_refs.insert(
                            value_id,
                            REValueInfo {
                                location: REValueLocation::BorrowedRoot(value_id.clone()),
                                visible: true,
                            },
                        );
                    }
                    _ => panic!("Unexpected"),
                }

                Ok((SNodeExecution::ValueRef(value_id), vec![]))
            }

            SNodeRef::VaultRef(vault_id) => {
                // Find value
                let value_id = ValueId::Vault(*vault_id);
                let cur_location = if self.owned_values.contains_key(&value_id) {
                    REValueLocation::OwnedRoot(value_id.clone())
                } else {
                    let maybe_value_ref = self.value_refs.get(&value_id);
                    maybe_value_ref
                        .map(|info| &info.location)
                        .cloned()
                        .ok_or(RuntimeError::ValueNotFound(ValueId::Vault(*vault_id)))?
                };

                // Lock values and setup next frame
                let next_location = {
                    // Lock Vault
                    let next_location = match cur_location {
                        REValueLocation::Track(address) => {
                            self.track
                                .take_lock(address.clone(), true)
                                .expect(&format!("Should never fail {:?}", address.clone()));
                            locked_values.insert(address.clone().into());
                            REValueLocation::Track(address)
                        }
                        REValueLocation::OwnedRoot(_)
                        | REValueLocation::Owned { .. }
                        | REValueLocation::Borrowed { .. } => {
                            let owned_ref = cur_location.to_owned_ref_mut(
                                &mut self.owned_values,
                                &mut self.frame_borrowed_values,
                            );
                            next_borrowed_values.insert(value_id.clone(), owned_ref);
                            REValueLocation::BorrowedRoot(value_id.clone())
                        }
                        _ => panic!("Unexpected vault location {:?}", cur_location),
                    };

                    // Lock Resource
                    let resource_address = {
                        let value_ref = next_location.to_ref(
                            &mut next_owned_values,
                            &mut next_borrowed_values,
                            &mut self.track,
                        );
                        value_ref.vault().resource_address()
                    };
                    self.track
                        .take_lock(resource_address, true)
                        .expect("Should never fail.");
                    locked_values.insert(resource_address.into());

                    next_location
                };

                // Retrieve Method Authorization
                let method_auth = {
                    let resource_address = {
                        let value_ref = next_location.to_ref(
                            &mut next_owned_values,
                            &mut next_borrowed_values,
                            &mut self.track,
                        );
                        value_ref.vault().resource_address()
                    };
                    let resource_manager =
                        self.track.read_value(resource_address).resource_manager();
                    resource_manager.get_vault_auth(&fn_ident).clone()
                };

                value_refs.insert(
                    value_id.clone(),
                    REValueInfo {
                        location: next_location,
                        visible: true,
                    },
                );

                Ok((SNodeExecution::ValueRef(value_id), vec![method_auth]))
            }
        }?;

        // Authorization check
        if !method_auths.is_empty() {
            let mut auth_zones = Vec::new();
            if let Some(self_auth_zone) = &self.auth_zone {
                auth_zones.push(self_auth_zone.borrow());
            }

            match &loaded_snode {
                // Resource auth check includes caller
                SNodeExecution::Scrypto(..)
                | SNodeExecution::ValueRef(ValueId::Resource(..), ..)
                | SNodeExecution::ValueRef(ValueId::Vault(..), ..)
                | SNodeExecution::Consumed(ValueId::Bucket(..)) => {
                    if let Some(auth_zone) = self.caller_auth_zone {
                        auth_zones.push(auth_zone.borrow());
                    }
                }
                // Extern call auth check
                _ => {}
            };

            let mut borrowed = Vec::new();
            for auth_zone in &auth_zones {
                borrowed.push(auth_zone.deref());
            }
            for method_auth in method_auths {
                method_auth
                    .check(&borrowed)
                    .map_err(|error| RuntimeError::AuthorizationError {
                        function: fn_ident.clone(),
                        authorization: method_auth,
                        error,
                    })?;
            }
        }

        // Prepare moving cost unit counter and fee table
        let cost_unit_counter = self
            .cost_unit_counter
            .take()
            .expect("Frame doesn't own a cost unit counter");
        let fee_table = self
            .fee_table
            .take()
            .expect("Frame doesn't own a fee table");

        // start a new frame
        let mut frame = CallFrame::new(
            self.transaction_hash,
            self.depth + 1,
            self.trace,
            self.track,
            self.wasm_engine,
            self.wasm_instrumenter,
            match loaded_snode {
                SNodeExecution::Scrypto(..)
                | SNodeExecution::Static(StaticSNodeState::TransactionProcessor) => {
                    Some(RefCell::new(AuthZone::new()))
                }
                _ => None,
            },
            match loaded_snode {
                SNodeExecution::Static(StaticSNodeState::TransactionProcessor) => {
                    Some(RefCell::new(Worktop::new()))
                }
                _ => None,
            },
            next_owned_values,
            value_refs,
            next_borrowed_values,
            self.auth_zone.as_ref(),
            cost_unit_counter,
            fee_table,
        );

        // invoke the main function
        let run_result = frame.run(Some(snode_ref), loaded_snode, &fn_ident, input);

        // re-gain ownership of the cost unit counter and fee table
        self.cost_unit_counter = frame.cost_unit_counter.take();
        self.fee_table = frame.fee_table.take();
        drop(frame);

        // unwrap and continue
        let (result, received_values) = run_result?;

        // Release locked addresses
        for l in locked_values {
            self.track.release_lock(l);
        }

        // move buckets and proofs to this process.
        for (id, value) in received_values {
            trace!(self, Level::Debug, "Received value: {:?}", value);
            self.owned_values.insert(id, RefCell::new(value));
        }

        Ok(result)
    }

    fn borrow_value(&self, value_id: &ValueId) -> REValueRef<'_, 'p, 's, S> {
        let info = self
            .value_refs
            .get(value_id)
            .expect(&format!("{:?} is unknown.", value_id));
        if !info.visible {
            panic!("Trying to read value which is not visible.")
        }

        info.location
            .to_ref(&self.owned_values, &self.frame_borrowed_values, &self.track)
    }

    fn borrow_value_mut(&mut self, value_id: &ValueId) -> RENativeValueRef<'p> {
        let info = self
            .value_refs
            .get(value_id)
            .expect(&format!("Value should exist {:?}", value_id));
        if !info.visible {
            panic!("Trying to read value which is not visible.")
        }

        info.location.borrow_native_ref(
            &mut self.owned_values,
            &mut self.frame_borrowed_values,
            &mut self.track,
        )
    }

    fn return_value_mut(&mut self, value_id: ValueId, val_ref: RENativeValueRef<'p>) {
        val_ref.return_to_location(
            value_id,
            &mut self.owned_values,
            &mut self.frame_borrowed_values,
            &mut self.track,
        )
    }

    fn drop_value(&mut self, value_id: &ValueId) -> REValue {
        self.owned_values.remove(&value_id).unwrap().into_inner()
    }

    fn create_value<V: Into<REValueByComplexity>>(
        &mut self,
        v: V,
    ) -> Result<ValueId, RuntimeError> {
        let value_by_complexity = v.into();
        let id = match value_by_complexity {
            REValueByComplexity::Primitive(REPrimitiveValue::Bucket(..)) => {
                let bucket_id = self.track.new_bucket_id();
                ValueId::Bucket(bucket_id)
            }
            REValueByComplexity::Primitive(REPrimitiveValue::Proof(..)) => {
                let proof_id = self.track.new_proof_id();
                ValueId::Proof(proof_id)
            }
            REValueByComplexity::Primitive(REPrimitiveValue::Vault(..)) => {
                let vault_id = self.track.new_vault_id();
                ValueId::Vault(vault_id)
            }
            REValueByComplexity::Primitive(REPrimitiveValue::KeyValue(..)) => {
                let kv_store_id = self.track.new_kv_store_id();
                ValueId::KeyValueStore(kv_store_id)
            }
            REValueByComplexity::Primitive(REPrimitiveValue::Package(..)) => {
                let package_address = self.track.new_package_address();
                ValueId::Package(package_address)
            }
            REValueByComplexity::Primitive(REPrimitiveValue::Resource(..)) => {
                let resource_address = self.track.new_resource_address();
                ValueId::Resource(resource_address)
            }
            REValueByComplexity::Primitive(REPrimitiveValue::NonFungibles(
                resource_address,
                ..,
            )) => ValueId::NonFungibles(resource_address),
            REValueByComplexity::Complex(REComplexValue::Component(..)) => {
                let component_address = self.track.new_component_address();
                ValueId::Component(component_address)
            }
        };

        let re_value = match value_by_complexity {
            REValueByComplexity::Primitive(primitive) => primitive.into(),
            REValueByComplexity::Complex(complex) => {
                let children = complex.get_children()?;
                let (child_values, mut missing) = self.take_available_values(children, true)?;
                let first_missing_value = missing.drain().nth(0);
                if let Some(missing_value) = first_missing_value {
                    return Err(RuntimeError::ValueNotFound(missing_value));
                }
                complex.into_re_value(child_values)
            }
        };
        self.owned_values.insert(id, RefCell::new(re_value));

        match id {
            ValueId::KeyValueStore(..) | ValueId::Resource(..) | ValueId::NonFungibles(..) => {
                self.value_refs.insert(
                    id.clone(),
                    REValueInfo {
                        location: REValueLocation::OwnedRoot(id.clone()),
                        visible: true,
                    },
                );
            }
            _ => {}
        }

        Ok(id)
    }

    fn globalize_value(&mut self, value_id: &ValueId) {
        let mut values = HashSet::new();
        values.insert(value_id.clone());
        let (taken_values, missing) = self.take_available_values(values, false).unwrap();
        assert!(missing.is_empty());
        assert!(taken_values.len() == 1);
        let value = taken_values.into_values().nth(0).unwrap();

        let (substate, maybe_child_values, maybe_non_fungibles) = match value {
            REValue::Component {
                component,
                child_values,
            } => (
                SubstateValue::Component(component),
                Some(child_values),
                None,
            ),
            REValue::Package(package) => (SubstateValue::Package(package), None, None),
            REValue::Resource(resource_manager) => {
                let non_fungibles =
                    if matches!(resource_manager.resource_type(), ResourceType::NonFungible) {
                        let resource_address: ResourceAddress = value_id.clone().into();
                        let re_value = self
                            .owned_values
                            .remove(&ValueId::NonFungibles(resource_address))
                            .unwrap()
                            .into_inner();
                        let non_fungibles: HashMap<NonFungibleId, NonFungible> = re_value.into();
                        Some(non_fungibles)
                    } else {
                        None
                    };
                (
                    SubstateValue::Resource(resource_manager),
                    None,
                    non_fungibles,
                )
            }
            _ => panic!("Not expected"),
        };

        let address = match value_id {
            ValueId::Component(component_address) => Address::GlobalComponent(*component_address),
            ValueId::Package(package_address) => Address::Package(*package_address),
            ValueId::Resource(resource_address) => Address::Resource(*resource_address),
            _ => panic!("Expected to be a component address"),
        };

        self.track.create_uuid_value(address.clone(), substate);

        if let Some(child_values) = maybe_child_values {
            let mut to_store_values = HashMap::new();
            for (id, cell) in child_values.into_iter() {
                to_store_values.insert(id, cell.into_inner());
            }
            self.track
                .insert_objects(to_store_values, address.clone().into());
        }

        if let Some(non_fungibles) = maybe_non_fungibles {
            let resource_address: ResourceAddress = address.clone().into();
            self.track
                .create_non_fungible_space(resource_address.clone());
            let parent_address = Address::NonFungibleSet(resource_address.clone());
            for (id, non_fungible) in non_fungibles {
                self.track.set_key_value(
                    parent_address.clone(),
                    id.to_vec(),
                    SubstateValue::NonFungible(Some(non_fungible)),
                );
            }
        }
    }

    fn remove_value_data(
        &mut self,
        address: SubstateAddress,
    ) -> Result<ScryptoValue, RuntimeError> {
        let (location, current_value) = self.read_value_internal(&address)?;
        let cur_children = current_value.value_ids();
        if !cur_children.is_empty() {
            return Err(RuntimeError::ValueNotAllowed);
        }

        // Write values
        let mut value_ref = location.to_ref_mut(
            &mut self.owned_values,
            &mut self.frame_borrowed_values,
            &mut self.track,
        );
        match address {
            SubstateAddress::Component(..) => {
                panic!("Should not get here");
            }
            SubstateAddress::KeyValueEntry(..) => {
                panic!("Should not get here");
            }
            SubstateAddress::NonFungible(.., id) => value_ref.non_fungible_remove(&id),
        }

        Ok(current_value)
    }

    fn read_value_data(&mut self, address: SubstateAddress) -> Result<ScryptoValue, RuntimeError> {
        let (parent_location, current_value) = self.read_value_internal(&address)?;
        let cur_children = current_value.value_ids();

        for child_id in cur_children {
            let child_location = match &address {
                SubstateAddress::Component(..) | SubstateAddress::NonFungible(..) => {
                    parent_location.child(AddressPath::ValueId(child_id))
                }
                SubstateAddress::KeyValueEntry(.., key) => parent_location
                    .child(AddressPath::Key(key.raw.clone()))
                    .child(AddressPath::ValueId(child_id)),
            };

            // Extend current readable space when kv stores are found
            let visible = matches!(child_id, ValueId::KeyValueStore(..));
            let child_info = REValueInfo {
                location: child_location,
                visible,
            };
            self.value_refs.insert(child_id, child_info);
        }
        Ok(current_value)
    }

    fn write_value_data(
        &mut self,
        address: SubstateAddress,
        value: ScryptoValue,
    ) -> Result<(), RuntimeError> {
        // If write, take values from current frame
        let (taken_values, missing) = {
            let value_ids = value.value_ids();
            match address {
                SubstateAddress::KeyValueEntry(..)
                | SubstateAddress::Component(_, ComponentOffset::State) => {
                    self.take_available_values(value_ids, true)?
                }
                SubstateAddress::Component(_, ComponentOffset::Info) => {
                    return Err(RuntimeError::InvalidDataWrite)
                }
                SubstateAddress::NonFungible(..) => {
                    if !value_ids.is_empty() {
                        return Err(RuntimeError::ValueNotAllowed);
                    }
                    (HashMap::new(), HashSet::new())
                }
            }
        };

        let (location, current_value) = self.read_value_internal(&address)?;
        let cur_children = current_value.value_ids();

        // Fulfill method
        verify_stored_value_update(&cur_children, &missing)?;

        // TODO: verify against some schema

        // Write values
        let mut pathed_values = HashMap::new();
        for (id, value) in taken_values {
            pathed_values.insert(AddressPath::ValueId(id), value);
        }
        let mut value_ref = location.to_ref_mut(
            &mut self.owned_values,
            &mut self.frame_borrowed_values,
            &mut self.track,
        );
        match address {
            SubstateAddress::Component(.., offset) => match offset {
                ComponentOffset::State => value_ref.component_put(value, pathed_values),
                ComponentOffset::Info => {
                    return Err(RuntimeError::InvalidDataWrite);
                }
            },
            SubstateAddress::KeyValueEntry(.., key) => {
                value_ref.kv_store_put(key.raw, value, pathed_values);
            }
            SubstateAddress::NonFungible(.., id) => value_ref.non_fungible_put(id, value),
        }

        Ok(())
    }

    fn get_epoch(&mut self) -> u64 {
        self.track.current_epoch()
    }

    fn get_transaction_hash(&mut self) -> Hash {
        self.track.transaction_hash()
    }

    fn generate_uuid(&mut self) -> u128 {
        self.track.new_uuid()
    }

    fn user_log(&mut self, level: Level, message: String) {
        self.track.add_log(level, message);
    }

    fn check_access_rule(
        &mut self,
        access_rule: scrypto::resource::AccessRule,
        proof_ids: Vec<ProofId>,
    ) -> Result<bool, RuntimeError> {
        let proofs = proof_ids
            .iter()
            .map(|proof_id| {
                self.owned_values
                    .get(&ValueId::Proof(*proof_id))
                    .map(|p| match p.borrow().deref() {
                        REValue::Proof(proof) => proof.clone(),
                        _ => panic!("Expected proof"),
                    })
                    .ok_or(RuntimeError::ProofNotFound(proof_id.clone()))
            })
            .collect::<Result<Vec<Proof>, RuntimeError>>()?;
        let mut simulated_auth_zone = AuthZone::new_with_proofs(proofs);

        let method_authorization = convert(&Type::Unit, &Value::Unit, &access_rule);
        let is_authorized = method_authorization.check(&[&simulated_auth_zone]).is_ok();
        simulated_auth_zone
            .main(
                "clear",
                ScryptoValue::from_typed(&AuthZoneClearInput {}),
                self,
            )
            .map_err(RuntimeError::AuthZoneError)?;

        Ok(is_authorized)
    }

    fn cost_unit_counter(&mut self) -> &mut CostUnitCounter {
        self.cost_unit_counter()
    }

    fn fee_table(&self) -> &FeeTable {
        self.fee_table()
    }
}
