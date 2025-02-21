use move_binary_format::errors::PartialVMResult;
use move_core_types::gas_algebra::{
    AbstractMemorySize, GasQuantity, InternalGas, InternalGasPerAbstractMemoryUnit,
};
use move_vm_runtime::native_functions::{NativeContext, NativeFunction};
use move_vm_types::loaded_data::runtime_types::Type;
use move_vm_types::natives::function::NativeResult;
use move_vm_types::pop_arg;
use move_vm_types::values::{Value, VectorRef};
use smallvec::smallvec;
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PushBackGasParameters {
    pub base: InternalGas,
    pub legacy_per_abstract_memory_unit: InternalGasPerAbstractMemoryUnit,
}

pub fn native_append(
    gas_params: &PushBackGasParameters,
    _context: &mut NativeContext,
    ty_args: Vec<Type>,
    mut args: VecDeque<Value>,
) -> PartialVMResult<NativeResult> {
    debug_assert!(ty_args.len() == 1);
    debug_assert!(args.len() == 2);

    let other = pop_arg!(args, VectorRef);
    let lhs = pop_arg!(args, VectorRef);
    let type_arg = &ty_args[0];
    let type_size = AbstractMemorySize::new(type_arg.num_nodes() as u64);

    other.reverse(type_arg)?;

    let mut cost = gas_params.base;
    let length = other.len(type_arg)?.value_as::<u64>()?;
    for _ in 0..length {
        let value = other.pop(type_arg)?;
        NativeResult::map_partial_vm_result_empty(cost, lhs.push_back(value, type_arg))?;
    }

    cost += gas_params.legacy_per_abstract_memory_unit
        * std::cmp::max(type_size, 1.into())
        * GasQuantity::new(length);

    Ok(NativeResult::ok(cost, smallvec![]))
}

pub fn make_native_append(gas_params: PushBackGasParameters) -> NativeFunction {
    Arc::new(
        move |context, ty_args, args| -> PartialVMResult<NativeResult> {
            native_append(&gas_params, context, ty_args, args)
        },
    )
}

pub fn make_all(
    gas_params: PushBackGasParameters,
) -> impl Iterator<Item = (String, NativeFunction)> {
    let natives = [("native_append", make_native_append(gas_params))];

    crate::natives::helpers::make_module_natives(natives)
}
