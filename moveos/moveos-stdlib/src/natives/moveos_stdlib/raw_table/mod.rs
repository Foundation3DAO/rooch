// Copyright (c) RoochNetwork
// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

/// A native Table implementation for save any type of value.
/// Refactor from https://github.com/rooch-network/move/blob/5b413d009515ad1144042ded27cbe9bd702aaad7/language/extensions/move-table-extension/src/lib.rs#L4
use better_any::{Tid, TidAble};
use move_binary_format::errors::{PartialVMError, PartialVMResult};
use move_core_types::{
    account_address::AccountAddress,
    effects::Op,
    gas_algebra::{InternalGas, InternalGasPerByte, NumBytes},
    language_storage::TypeTag,
    value::MoveTypeLayout,
    vm_status::StatusCode,
};
use move_vm_runtime::{
    native_functions,
    native_functions::{NativeContext, NativeFunction, NativeFunctionTable},
};
use move_vm_types::{
    loaded_data::runtime_types::Type,
    natives::function::NativeResult,
    pop_arg,
    values::{GlobalValue, Value},
};
use moveos_types::object::ObjectID;
use serde::{Deserialize, Serialize};
use smallvec::smallvec;
use std::{
    cell::RefCell,
    collections::{btree_map::Entry, BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

// ===========================================================================================
// Public Data Structures and Constants

/// The representation of a table handle. This is created from truncating a sha3-256 based
/// hash over a transaction hash provided by the environment and a table creation counter
/// local to the transaction.
#[derive(Copy, Clone, Debug, PartialOrd, Ord, PartialEq, Eq)]
pub struct TableHandle(pub AccountAddress);

impl std::fmt::Display for TableHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "T-{:X}", self.0)
    }
}

impl From<TableHandle> for ObjectID {
    fn from(table_handle: TableHandle) -> Self {
        table_handle.0.into()
    }
}

#[derive(Clone, Debug)]
pub struct TableInfo {
    pub key_type: TypeTag,
}

impl TableInfo {
    pub fn new(key_type: TypeTag) -> Self {
        Self { key_type }
    }
}

impl std::fmt::Display for TableInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Table<{}>", self.key_type)
    }
}

/// A table change set.
#[derive(Default)]
pub struct TableChangeSet {
    pub new_tables: BTreeMap<TableHandle, TableInfo>,
    pub removed_tables: BTreeSet<TableHandle>,
    pub changes: BTreeMap<TableHandle, TableChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValueBox {
    pub value_tag: TypeTag,
    pub value: Vec<u8>,
}

/// A change of a single table.
pub struct TableChange {
    pub entries: BTreeMap<Vec<u8>, Op<Vec<u8>>>,
}

/// A table resolver which needs to be provided by the environment. This allows to lookup
/// data in remote storage, as well as retrieve cost of table operations.
pub trait TableResolver {
    fn resolve_table_entry(
        &self,
        handle: &TableHandle,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, anyhow::Error>;
}

/// The native table context extension. This needs to be attached to the NativeContextExtensions
/// value which is passed into session functions, so its accessible from natives of this
/// extension.
#[derive(Tid)]
pub struct NativeTableContext<'a> {
    resolver: &'a dyn TableResolver,
    //txn_hash: [u8; 32],
    table_data: RefCell<TableData>,
}

// See stdlib/Error.move
const _ECATEGORY_INVALID_STATE: u8 = 0;
const ECATEGORY_INVALID_ARGUMENT: u8 = 7;

const ALREADY_EXISTS: u64 = (100 << 8) + ECATEGORY_INVALID_ARGUMENT as u64;
const NOT_FOUND: u64 = (101 << 8) + ECATEGORY_INVALID_ARGUMENT as u64;
// Move side raises this
const _NOT_EMPTY: u64 = (102 << 8) + _ECATEGORY_INVALID_STATE as u64;

// ===========================================================================================
// Private Data Structures and Constants

/// A structure representing mutable data of the NativeTableContext. This is in a RefCell
/// of the overall context so we can mutate while still accessing the overall context.
#[derive(Default)]
struct TableData {
    new_tables: BTreeMap<TableHandle, TableInfo>,
    removed_tables: BTreeSet<TableHandle>,
    tables: BTreeMap<TableHandle, Table>,
}

/// A structure representing table value.
struct TableValue {
    value_layout: MoveTypeLayout,
    value: GlobalValue,
}

/// A structure representing a single table.
struct Table {
    handle: TableHandle,
    key_layout: MoveTypeLayout,
    content: BTreeMap<Vec<u8>, TableValue>,
}

// =========================================================================================
// Implementation of Native Table Context

impl<'a> NativeTableContext<'a> {
    /// Create a new instance of a native table context. This must be passed in via an
    /// extension into VM session functions.
    pub fn new(resolver: &'a dyn TableResolver) -> Self {
        Self {
            resolver,
            table_data: Default::default(),
        }
    }

    /// Computes the change set from a NativeTableContext.
    pub fn into_change_set(self) -> PartialVMResult<TableChangeSet> {
        let NativeTableContext { table_data, .. } = self;
        let TableData {
            new_tables,
            removed_tables,
            tables,
        } = table_data.into_inner();
        let mut changes = BTreeMap::new();
        for (handle, table) in tables {
            let Table { content, .. } = table;
            let mut entries = BTreeMap::new();
            for (
                key,
                TableValue {
                    value_layout,
                    value: gv,
                },
            ) in content
            {
                let op = match gv.into_effect() {
                    Some(op) => op,
                    None => continue,
                };
                //let value_tag: TypeTag = (&value_layout).try_into().map_err(|_|PartialVMError::new(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR))?;
                match op {
                    Op::New(val) => {
                        let bytes = serialize(&value_layout, &val)?;
                        //let value_box = ValueBox{ value_tag, value: bytes};
                        entries.insert(key, Op::New(bytes));
                    }
                    Op::Modify(val) => {
                        let bytes = serialize(&value_layout, &val)?;
                        //let value_box = ValueBox{ value_tag, value: bytes};
                        entries.insert(key, Op::Modify(bytes));
                    }
                    Op::Delete => {
                        entries.insert(key, Op::Delete);
                    }
                }
            }
            if !entries.is_empty() {
                changes.insert(handle, TableChange { entries });
            }
        }
        Ok(TableChangeSet {
            new_tables,
            removed_tables,
            changes,
        })
    }
}

impl TableData {
    /// Gets or creates a new table in the TableData. This initializes information about
    /// the table, like the type layout for keys and values.
    fn get_or_create_table(
        &mut self,
        context: &NativeContext,
        handle: TableHandle,
        key_ty: &Type,
    ) -> PartialVMResult<&mut Table> {
        Ok(match self.tables.entry(handle) {
            Entry::Vacant(e) => {
                let key_layout = get_type_layout(context, key_ty)?;
                let table = Table {
                    handle,
                    key_layout,
                    content: Default::default(),
                };
                e.insert(table)
            }
            Entry::Occupied(e) => e.into_mut(),
        })
    }
}

impl Table {
    fn get_or_create_global_value(
        &mut self,
        native_context: &NativeContext,
        table_context: &NativeTableContext,
        key: Vec<u8>,
        value_type: &Type,
    ) -> PartialVMResult<(&mut GlobalValue, Option<Option<NumBytes>>)> {
        Ok(match self.content.entry(key) {
            Entry::Vacant(entry) => {
                let value_layout = get_type_layout(native_context, value_type)?;
                let (gv, loaded) = match table_context
                    .resolver
                    .resolve_table_entry(&self.handle, entry.key())
                    .map_err(|err| {
                        partial_extension_error(format!("remote table resolver failure: {}", err))
                    })? {
                    Some(val_bytes) => {
                        let val = deserialize(&value_layout, &val_bytes)?;
                        (
                            GlobalValue::cached(val)?,
                            Some(NumBytes::new(val_bytes.len() as u64)),
                        )
                    }
                    None => (GlobalValue::none(), None),
                };
                (
                    &mut entry
                        .insert(TableValue {
                            value_layout,
                            value: gv,
                        })
                        .value,
                    Some(loaded),
                )
            }
            Entry::Occupied(entry) => (&mut entry.into_mut().value, None),
        })
    }
}

// =========================================================================================
// Native Function Implementations

/// Returns all natives for tables.
pub fn table_natives(table_addr: AccountAddress, gas_params: GasParameters) -> NativeFunctionTable {
    let natives: [(&str, &str, NativeFunction); 7] = [
        (
            "raw_table",
            "add_box",
            make_native_add_box(gas_params.common.clone(), gas_params.add_box),
        ),
        (
            "raw_table",
            "borrow_box",
            make_native_borrow_box(gas_params.common.clone(), gas_params.borrow_box.clone()),
        ),
        (
            "raw_table",
            "borrow_box_mut",
            make_native_borrow_box(gas_params.common.clone(), gas_params.borrow_box),
        ),
        (
            "raw_table",
            "remove_box",
            make_native_remove_box(gas_params.common.clone(), gas_params.remove_box),
        ),
        (
            "raw_table",
            "contains_box",
            make_native_contains_box(gas_params.common, gas_params.contains_box),
        ),
        (
            "raw_table",
            "destroy_empty_box",
            make_native_destroy_empty_box(gas_params.destroy_empty_box),
        ),
        (
            "raw_table",
            "drop_unchecked_box",
            make_native_drop_unchecked_box(gas_params.drop_unchecked_box),
        ),
    ];

    native_functions::make_table_from_iter(table_addr, natives)
}

#[derive(Debug, Clone)]
pub struct CommonGasParameters {
    pub load_base: InternalGas,
    pub load_per_byte: InternalGasPerByte,
    pub load_failure: InternalGas,
}

impl CommonGasParameters {
    fn calculate_load_cost(&self, loaded: Option<Option<NumBytes>>) -> InternalGas {
        self.load_base
            + match loaded {
                Some(Some(num_bytes)) => self.load_per_byte * num_bytes,
                Some(None) => self.load_failure,
                None => 0.into(),
            }
    }
}

#[derive(Debug, Clone)]
pub struct AddBoxGasParameters {
    pub base: InternalGas,
    pub per_byte_serialized: InternalGasPerByte,
}

fn native_add_box(
    common_gas_params: &CommonGasParameters,
    gas_params: &AddBoxGasParameters,
    context: &mut NativeContext,
    ty_args: Vec<Type>,
    mut args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    assert_eq!(ty_args.len(), 3);
    assert_eq!(args.len(), 3);

    let table_context = context.extensions().get::<NativeTableContext>();
    let mut table_data = table_context.table_data.borrow_mut();

    let val = args.pop_back().unwrap();
    let key = args.pop_back().unwrap();
    let handle = get_table_handle(pop_arg!(args, AccountAddress))?;

    let mut cost = gas_params.base;

    let table = table_data.get_or_create_table(context, handle, &ty_args[0])?;

    let key_bytes = serialize(&table.key_layout, &key)?;
    cost += gas_params.per_byte_serialized * NumBytes::new(key_bytes.len() as u64);

    let (gv, loaded) =
        table.get_or_create_global_value(context, table_context, key_bytes, &ty_args[2])?;
    cost += common_gas_params.calculate_load_cost(loaded);

    match gv.move_to(val) {
        Ok(_) => Ok(NativeResult::ok(cost, smallvec![])),
        Err(_) => Ok(NativeResult::err(cost, ALREADY_EXISTS)),
    }
}

pub fn make_native_add_box(
    common_gas_params: CommonGasParameters,
    gas_params: AddBoxGasParameters,
) -> NativeFunction {
    Arc::new(
        move |context, ty_args, args| -> PartialVMResult<NativeResult> {
            native_add_box(&common_gas_params, &gas_params, context, ty_args, args)
        },
    )
}

#[derive(Debug, Clone)]
pub struct BorrowBoxGasParameters {
    pub base: InternalGas,
    pub per_byte_serialized: InternalGasPerByte,
}

fn native_borrow_box(
    common_gas_params: &CommonGasParameters,
    gas_params: &BorrowBoxGasParameters,
    context: &mut NativeContext,
    ty_args: Vec<Type>,
    mut args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    assert_eq!(ty_args.len(), 3);
    assert_eq!(args.len(), 2);

    let table_context = context.extensions().get::<NativeTableContext>();
    let mut table_data = table_context.table_data.borrow_mut();

    let key = args.pop_back().unwrap();
    let handle = get_table_handle(pop_arg!(args, AccountAddress))?;

    let table = table_data.get_or_create_table(context, handle, &ty_args[0])?;

    let mut cost = gas_params.base;

    let key_bytes = serialize(&table.key_layout, &key)?;
    cost += gas_params.per_byte_serialized * NumBytes::new(key_bytes.len() as u64);

    let (gv, loaded) =
        table.get_or_create_global_value(context, table_context, key_bytes, &ty_args[2])?;
    cost += common_gas_params.calculate_load_cost(loaded);

    match gv.borrow_global() {
        Ok(ref_val) => Ok(NativeResult::ok(cost, smallvec![ref_val])),
        Err(_) => Ok(NativeResult::err(cost, NOT_FOUND)),
    }
}

pub fn make_native_borrow_box(
    common_gas_params: CommonGasParameters,
    gas_params: BorrowBoxGasParameters,
) -> NativeFunction {
    Arc::new(
        move |context, ty_args, args| -> PartialVMResult<NativeResult> {
            native_borrow_box(&common_gas_params, &gas_params, context, ty_args, args)
        },
    )
}

#[derive(Debug, Clone)]
pub struct ContainsBoxGasParameters {
    pub base: InternalGas,
    pub per_byte_serialized: InternalGasPerByte,
}

fn native_contains_box(
    common_gas_params: &CommonGasParameters,
    gas_params: &ContainsBoxGasParameters,
    context: &mut NativeContext,
    ty_args: Vec<Type>,
    mut args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    assert_eq!(ty_args.len(), 3);
    assert_eq!(args.len(), 2);

    let table_context = context.extensions().get::<NativeTableContext>();
    let mut table_data = table_context.table_data.borrow_mut();

    let key = args.pop_back().unwrap();
    let handle = get_table_handle(pop_arg!(args, AccountAddress))?;

    let table = table_data.get_or_create_table(context, handle, &ty_args[0])?;

    let mut cost = gas_params.base;

    let key_bytes = serialize(&table.key_layout, &key)?;
    cost += gas_params.per_byte_serialized * NumBytes::new(key_bytes.len() as u64);

    let (gv, loaded) =
        table.get_or_create_global_value(context, table_context, key_bytes, &ty_args[2])?;
    cost += common_gas_params.calculate_load_cost(loaded);

    let exists = Value::bool(gv.exists()?);

    Ok(NativeResult::ok(cost, smallvec![exists]))
}

pub fn make_native_contains_box(
    common_gas_params: CommonGasParameters,
    gas_params: ContainsBoxGasParameters,
) -> NativeFunction {
    Arc::new(
        move |context, ty_args, args| -> PartialVMResult<NativeResult> {
            native_contains_box(&common_gas_params, &gas_params, context, ty_args, args)
        },
    )
}

#[derive(Debug, Clone)]
pub struct RemoveGasParameters {
    pub base: InternalGas,
    pub per_byte_serialized: InternalGasPerByte,
}

fn native_remove_box(
    common_gas_params: &CommonGasParameters,
    gas_params: &RemoveGasParameters,
    context: &mut NativeContext,
    ty_args: Vec<Type>,
    mut args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    assert_eq!(ty_args.len(), 3);
    assert_eq!(args.len(), 2);

    let table_context = context.extensions().get::<NativeTableContext>();
    let mut table_data = table_context.table_data.borrow_mut();

    let key = args.pop_back().unwrap();
    let handle = get_table_handle(pop_arg!(args, AccountAddress))?;

    let table = table_data.get_or_create_table(context, handle, &ty_args[0])?;

    let mut cost = gas_params.base;

    let key_bytes = serialize(&table.key_layout, &key)?;
    cost += gas_params.per_byte_serialized * NumBytes::new(key_bytes.len() as u64);
    let (gv, loaded) =
        table.get_or_create_global_value(context, table_context, key_bytes, &ty_args[2])?;
    cost += common_gas_params.calculate_load_cost(loaded);

    match gv.move_from() {
        Ok(val) => Ok(NativeResult::ok(cost, smallvec![val])),
        Err(_) => Ok(NativeResult::err(cost, NOT_FOUND)),
    }
}

pub fn make_native_remove_box(
    common_gas_params: CommonGasParameters,
    gas_params: RemoveGasParameters,
) -> NativeFunction {
    Arc::new(
        move |context, ty_args, args| -> PartialVMResult<NativeResult> {
            native_remove_box(&common_gas_params, &gas_params, context, ty_args, args)
        },
    )
}

#[derive(Debug, Clone)]
pub struct DestroyEmptyBoxGasParameters {
    pub base: InternalGas,
}

fn native_destroy_empty_box(
    gas_params: &DestroyEmptyBoxGasParameters,
    context: &mut NativeContext,
    _ty_args: Vec<Type>,
    mut args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    assert_eq!(args.len(), 1);

    let table_context = context.extensions().get::<NativeTableContext>();
    let mut table_data = table_context.table_data.borrow_mut();

    let handle = get_table_handle(pop_arg!(args, AccountAddress))?;
    assert!(table_data.removed_tables.insert(handle));

    Ok(NativeResult::ok(gas_params.base, smallvec![]))
}

pub fn make_native_destroy_empty_box(gas_params: DestroyEmptyBoxGasParameters) -> NativeFunction {
    Arc::new(
        move |context, ty_args, args| -> PartialVMResult<NativeResult> {
            native_destroy_empty_box(&gas_params, context, ty_args, args)
        },
    )
}

#[derive(Debug, Clone)]
pub struct DropUncheckedBoxGasParameters {
    pub base: InternalGas,
}

fn native_drop_unchecked_box(
    gas_params: &DropUncheckedBoxGasParameters,
    _context: &mut NativeContext,
    _ty_args: Vec<Type>,
    args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    assert_eq!(args.len(), 1);

    Ok(NativeResult::ok(gas_params.base, smallvec![]))
}

pub fn make_native_drop_unchecked_box(gas_params: DropUncheckedBoxGasParameters) -> NativeFunction {
    Arc::new(
        move |context, ty_args, args| -> PartialVMResult<NativeResult> {
            native_drop_unchecked_box(&gas_params, context, ty_args, args)
        },
    )
}

#[derive(Debug, Clone)]
pub struct GasParameters {
    pub common: CommonGasParameters,
    pub add_box: AddBoxGasParameters,
    pub borrow_box: BorrowBoxGasParameters,
    pub contains_box: ContainsBoxGasParameters,
    pub remove_box: RemoveGasParameters,
    pub destroy_empty_box: DestroyEmptyBoxGasParameters,
    pub drop_unchecked_box: DropUncheckedBoxGasParameters,
}

impl GasParameters {
    pub fn zeros() -> Self {
        Self {
            common: CommonGasParameters {
                load_base: 0.into(),
                load_per_byte: 0.into(),
                load_failure: 0.into(),
            },
            add_box: AddBoxGasParameters {
                base: 0.into(),
                per_byte_serialized: 0.into(),
            },
            borrow_box: BorrowBoxGasParameters {
                base: 0.into(),
                per_byte_serialized: 0.into(),
            },
            contains_box: ContainsBoxGasParameters {
                base: 0.into(),
                per_byte_serialized: 0.into(),
            },
            remove_box: RemoveGasParameters {
                base: 0.into(),
                per_byte_serialized: 0.into(),
            },
            destroy_empty_box: DestroyEmptyBoxGasParameters { base: 0.into() },
            drop_unchecked_box: DropUncheckedBoxGasParameters { base: 0.into() },
        }
    }
}

// =========================================================================================
// Helpers

fn get_table_handle(handle: AccountAddress) -> PartialVMResult<TableHandle> {
    Ok(TableHandle(handle))
}

fn serialize(layout: &MoveTypeLayout, val: &Value) -> PartialVMResult<Vec<u8>> {
    val.simple_serialize(layout)
        .ok_or_else(|| partial_extension_error("cannot serialize table key or value"))
}

fn deserialize(layout: &MoveTypeLayout, bytes: &[u8]) -> PartialVMResult<Value> {
    Value::simple_deserialize(bytes, layout)
        .ok_or_else(|| partial_extension_error("cannot deserialize table key or value"))
}

fn partial_extension_error(msg: impl ToString) -> PartialVMError {
    PartialVMError::new(StatusCode::VM_EXTENSION_ERROR).with_message(msg.to_string())
}

fn get_type_layout(context: &NativeContext, ty: &Type) -> PartialVMResult<MoveTypeLayout> {
    context
        .type_to_type_layout(ty)?
        .ok_or_else(|| partial_extension_error("cannot determine type layout"))
}
