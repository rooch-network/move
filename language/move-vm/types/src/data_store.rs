// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    loaded_data::runtime_types::Type,
    values::{GlobalValue, Value},
};
use move_binary_format::errors::{PartialVMResult, VMResult};
use move_core_types::{
    account_address::AccountAddress,
    effects::{ChangeSet, Event},
    gas_algebra::NumBytes,
    language_storage::ModuleId,
    value::MoveTypeLayout,
};

/// Provide an implementation for bytecodes related to data with a given data store.
///
/// The `DataStore` is a generic concept that includes both data and events.
/// A default implementation of the `DataStore` is `TransactionDataCache` which provides
/// an in memory cache for a given transaction and the atomic transactional changes
/// proper of a script execution (transaction).
pub trait DataStore {
    // ---
    // StateStore operations
    // ---

    /// Try to load a resource from remote storage and create a corresponding GlobalValue
    /// that is owned by the data store.
    fn load_resource(
        &mut self,
        addr: AccountAddress,
        ty: &Type,
    ) -> PartialVMResult<(&mut GlobalValue, Option<Option<NumBytes>>)>;

    /// Get the serialized format of a `CompiledModule` given a `ModuleId`.
    fn load_module(&self, module_id: &ModuleId) -> VMResult<Vec<u8>>;

    /// Publish a module.
    fn publish_module(
        &mut self,
        module_id: &ModuleId,
        blob: Vec<u8>,
        is_republishing: bool,
    ) -> VMResult<()>;

    /// Check if this module exists.
    fn exists_module(&self, module_id: &ModuleId) -> VMResult<bool>;

    // ---
    // EventStore operations
    // ---

    /// Emit an event to the EventStore
    fn emit_event(
        &mut self,
        guid: Vec<u8>,
        seq_num: u64,
        ty: Type,
        val: Value,
    ) -> PartialVMResult<()>;

    fn events(&self) -> &Vec<(Vec<u8>, u64, Type, MoveTypeLayout, Value)>;
}

/// Trait for transaction data cache. Keep updates within a transaction so they can all be published at
/// once when the transaction succeeds.
///
/// It also provides an implementation for the opcodes that refer to storage and gives the
/// proper guarantees of reference lifetime.
///
/// Dirty objects are serialized and returned in make_write_set.
///
/// It is a responsibility of the client to publish changes once the transaction is executed.
pub trait TransactionCache {
    /// Make a write set from the updated (dirty, deleted) global resources along with
    /// published modules.
    ///
    /// Gives all proper guarantees on lifetime of global data as well.
    fn into_effects(self) -> PartialVMResult<(ChangeSet, Vec<Event>)>;
    fn num_mutated_accounts(&self, sender: &AccountAddress) -> u64;
}
