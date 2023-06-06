//! ACVM simulator is independent of the proving backend against which the ACIR code is being proven.
//! However there are currently a few opcodes for which there is currently no rust implementation so we must
//! use the C++ implementations included in Aztec Lab's Barretenberg library.
//!
//! As [`acvm`] includes rust implementations for these opcodes, this module can be removed.

mod barretenberg_structures;
pub(crate) mod pedersen;
pub(crate) mod scalar_mul;
pub(crate) mod schnorr;

use barretenberg_structures::Assignments;

/// The number of bytes necessary to store a `FieldElement`.
const FIELD_BYTES: usize = 32;

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    FromFeature(#[from] FeatureError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FeatureError {
    #[error("Trying to call {name} resulted in an error")]
    FunctionCallFailed { name: String, source: wasmer::RuntimeError },
    #[error("Could not find function export named {name}")]
    InvalidExport { name: String, source: wasmer::ExportError },
    #[error("No value available when value was expected")]
    NoValue,
    #[error("Value expected to be i32")]
    InvalidI32,
    #[error("Could not convert value {value} from i32 to u32")]
    InvalidU32 { value: i32, source: std::num::TryFromIntError },
    #[error("Could not convert value {value} from i32 to usize")]
    InvalidUsize { value: i32, source: std::num::TryFromIntError },
    #[error("Value expected to be 0 or 1 representing a boolean")]
    InvalidBool,
}
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(crate) struct BackendError(#[from] Error);

impl From<FeatureError> for BackendError {
    fn from(value: FeatureError) -> Self {
        value.into()
    }
}

#[derive(Debug)]
pub(crate) struct Barretenberg {
    store: std::cell::RefCell<wasmer::Store>,
    memory: wasmer::Memory,
    instance: wasmer::Instance,
}

impl Default for Barretenberg {
    fn default() -> Barretenberg {
        Barretenberg::new()
    }
}

#[test]
fn smoke() -> Result<(), Error> {
    use crate::pedersen::Pedersen;

    let b = Barretenberg::new();
    let (x, y) = b.encrypt(vec![acvm::FieldElement::zero(), acvm::FieldElement::one()])?;
    dbg!(x.to_hex(), y.to_hex());
    Ok(())
}

mod wasm {
    use std::cell::RefCell;
    use wasmer::{
        imports, Function, FunctionEnv, FunctionEnvMut, Instance, Memory, MemoryType, Module,
        Store, Value, WasmPtr,
    };

    use super::{Barretenberg, Error, FeatureError};

    /// The number of bytes necessary to represent a pointer to memory inside the wasm.
    // pub(super) const POINTER_BYTES: usize = 4;

    /// The Barretenberg WASM gives us 1024 bytes of scratch space which we can use without
    /// needing to allocate/free it ourselves. This can be useful for when we need to pass in several small variables
    /// when calling functions on the wasm, however it's important to not overrun this scratch space as otherwise
    /// the written data will begin to corrupt the stack.
    ///
    /// Using this scratch space isn't particularly safe if we have multiple threads interacting with the wasm however,
    /// each thread could write to the same pointer address simultaneously.
    pub(super) const WASM_SCRATCH_BYTES: usize = 1024;

    /// Embed the Barretenberg WASM file
    #[derive(rust_embed::RustEmbed)]
    #[folder = "$BARRETENBERG_BIN_DIR"]
    #[include = "barretenberg.wasm"]
    struct Wasm;

    impl Barretenberg {
        pub(crate) fn new() -> Barretenberg {
            let (instance, memory, store) = instance_load();
            Barretenberg { memory, instance, store: RefCell::new(store) }
        }
    }

    /// A wrapper around the arguments or return value from a WASM call.
    /// Notice, `Option<Value>` is used because not every call returns a value,
    /// some calls are simply made to free a pointer or manipulate the heap.
    #[derive(Debug, Clone)]
    pub(crate) struct WASMValue(Option<Value>);

    impl From<usize> for WASMValue {
        fn from(value: usize) -> Self {
            WASMValue(Some(Value::I32(value as i32)))
        }
    }

    impl From<i32> for WASMValue {
        fn from(value: i32) -> Self {
            WASMValue(Some(Value::I32(value)))
        }
    }

    impl From<Value> for WASMValue {
        fn from(value: Value) -> Self {
            WASMValue(Some(value))
        }
    }

    impl TryFrom<WASMValue> for bool {
        type Error = FeatureError;

        fn try_from(value: WASMValue) -> Result<Self, Self::Error> {
            match value.try_into()? {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(FeatureError::InvalidBool),
            }
        }
    }

    impl TryFrom<WASMValue> for usize {
        type Error = FeatureError;

        fn try_from(value: WASMValue) -> Result<Self, Self::Error> {
            let value: i32 = value.try_into()?;
            value.try_into().map_err(|source| FeatureError::InvalidUsize { value, source })
        }
    }

    impl TryFrom<WASMValue> for u32 {
        type Error = FeatureError;

        fn try_from(value: WASMValue) -> Result<Self, Self::Error> {
            let value = value.try_into()?;
            u32::try_from(value).map_err(|source| FeatureError::InvalidU32 { value, source })
        }
    }

    impl TryFrom<WASMValue> for i32 {
        type Error = FeatureError;

        fn try_from(value: WASMValue) -> Result<Self, Self::Error> {
            value
                .0
                .map_or(Err(FeatureError::NoValue), |val| val.i32().ok_or(FeatureError::InvalidI32))
        }
    }

    impl TryFrom<WASMValue> for Value {
        type Error = FeatureError;

        fn try_from(value: WASMValue) -> Result<Self, Self::Error> {
            value.0.ok_or(FeatureError::NoValue)
        }
    }

    impl Barretenberg {
        /// Transfer bytes to WASM heap
        // TODO: Consider making this Result-returning
        pub(crate) fn transfer_to_heap(&self, data: &[u8], offset: usize) {
            let memory = &self.memory;
            let store = self.store.borrow();
            let memory_view = memory.view(&store);

            memory_view.write(offset as u64, data).unwrap()
        }

        // TODO: Consider making this Result-returning
        pub(crate) fn read_memory<const SIZE: usize>(&self, start: usize) -> [u8; SIZE] {
            self.read_memory_variable_length(start, SIZE)
                .try_into()
                .expect("Read memory should be of the specified length")
        }

        // TODO: Consider making this Result-returning
        pub(crate) fn read_memory_variable_length(&self, offset: usize, length: usize) -> Vec<u8> {
            let memory = &self.memory;
            let store = &self.store.borrow();
            let memory_view = memory.view(&store);

            let mut buf = vec![0; length];

            memory_view.read(offset as u64, &mut buf).unwrap();
            buf
        }

        // pub(crate) fn get_pointer(&self, ptr_ptr: usize) -> usize {
        //     let ptr: [u8; POINTER_BYTES] = self.read_memory(ptr_ptr);
        //     u32::from_le_bytes(ptr) as usize
        // }

        pub(crate) fn call(&self, name: &str, param: &WASMValue) -> Result<WASMValue, Error> {
            self.call_multiple(name, vec![param])
        }

        pub(crate) fn call_multiple(
            &self,
            name: &str,
            params: Vec<&WASMValue>,
        ) -> Result<WASMValue, Error> {
            // We take in a reference to values, since they do not implement Copy.
            // We then clone them inside of this function, so that the API does not have a bunch of Clones everywhere

            let mut args: Vec<Value> = vec![];
            for param in params.into_iter().cloned() {
                args.push(param.try_into()?)
            }
            let func =
                self.instance.exports.get_function(name).map_err(|source| {
                    FeatureError::InvalidExport { name: name.to_string(), source }
                })?;
            let boxed_value = func.call(&mut self.store.borrow_mut(), &args).map_err(|source| {
                FeatureError::FunctionCallFailed { name: name.to_string(), source }
            })?;
            let option_value = boxed_value.first().cloned();

            Ok(WASMValue(option_value))
        }

        /// Creates a pointer and allocates the bytes that the pointer references to, to the heap
        pub(crate) fn allocate(&self, bytes: &[u8]) -> Result<WASMValue, Error> {
            let ptr: i32 = self.call("bbmalloc", &bytes.len().into())?.try_into()?;

            let i32_bytes = ptr.to_be_bytes();
            let u32_bytes = u32::from_be_bytes(i32_bytes);

            self.transfer_to_heap(bytes, u32_bytes as usize);
            Ok(ptr.into())
        }

        // pub(super) fn free(&self, pointer: WASMValue) -> Result<(), Error> {
        //     self.call("bbfree", &pointer)?;
        //     Ok(())
        // }
    }

    fn instance_load() -> (Instance, Memory, Store) {
        let mut store = Store::default();

        let module = Module::new(&store, Wasm::get("barretenberg.wasm").unwrap().data).unwrap();

        let mem_type = MemoryType::new(22, None, false);
        let memory = Memory::new(&mut store, mem_type).unwrap();

        let function_env = FunctionEnv::new(&mut store, memory.clone());

        let custom_imports = imports! {
            "env" => {
                "logstr" => Function::new_typed_with_env(
                    &mut store,
                    &function_env,
                    logstr,
                ),
                "set_data" => Function::new_typed(&mut store, set_data),
                "get_data" => Function::new_typed(&mut store, get_data),
                "env_load_verifier_crs" => Function::new_typed(&mut store, env_load_verifier_crs),
                "env_load_prover_crs" => Function::new_typed(&mut store, env_load_prover_crs),
                "memory" => memory.clone(),
            },
            "wasi_snapshot_preview1" => {
                "fd_read" => Function::new_typed(&mut store, fd_read),
                "fd_close" => Function::new_typed(&mut store, fd_close),
                "proc_exit" =>  Function::new_typed(&mut store, proc_exit),
                "fd_fdstat_get" => Function::new_typed(&mut store, fd_fdstat_get),
                "random_get" => Function::new_typed_with_env(
                    &mut store,
                    &function_env,
                    random_get
                ),
                "fd_seek" => Function::new_typed(&mut store, fd_seek),
                "fd_write" => Function::new_typed(&mut store, fd_write),
                "environ_sizes_get" => Function::new_typed(&mut store, environ_sizes_get),
                "environ_get" => Function::new_typed(&mut store, environ_get),
                "clock_time_get" => Function::new_typed(&mut store, clock_time_get),
            },
        };

        (Instance::new(&mut store, &module, &custom_imports).unwrap(), memory, store)
    }

    fn logstr(mut env: FunctionEnvMut<Memory>, ptr: i32) {
        let (memory, store) = env.data_and_store_mut();
        let memory_view = memory.view(&store);

        let log_str_wasm_ptr: WasmPtr<u8, wasmer::Memory32> = WasmPtr::new(ptr as u32);

        match log_str_wasm_ptr.read_utf8_string_with_nul(&memory_view) {
            Ok(log_string) => println!("{log_string}"),
            Err(err) => println!("Error while reading log string from memory: {err}"),
        };
    }

    // Based on https://github.com/wasmerio/wasmer/blob/2.3.0/lib/wasi/src/syscalls/mod.rs#L2537
    fn random_get(mut env: FunctionEnvMut<Memory>, buf_ptr: i32, buf_len: i32) -> i32 {
        let mut u8_buffer = vec![0; buf_len as usize];
        let res = getrandom::getrandom(&mut u8_buffer);
        match res {
            Ok(()) => {
                let (memory, store) = env.data_and_store_mut();
                let memory_view = memory.view(&store);
                match memory_view.write(buf_ptr as u64, u8_buffer.as_mut_slice()) {
                    Ok(_) => {
                        0_i32 // __WASI_ESUCCESS
                    }
                    Err(_) => {
                        29_i32 // __WASI_EIO
                    }
                }
            }
            Err(_) => {
                29_i32 // __WASI_EIO
            }
        }
    }

    fn clock_time_get(_: i32, _: i64, _: i32) -> i32 {
        unimplemented!("proc_exit: clock_time_get is not implemented")
    }

    fn proc_exit(_: i32) {
        unimplemented!("proc_exit is not implemented")
    }

    fn fd_write(_: i32, _: i32, _: i32, _: i32) -> i32 {
        unimplemented!("fd_write is not implemented")
    }

    fn fd_seek(_: i32, _: i64, _: i32, _: i32) -> i32 {
        unimplemented!("fd_seek is not implemented")
    }

    fn fd_read(_: i32, _: i32, _: i32, _: i32) -> i32 {
        unimplemented!("fd_read is not implemented")
    }

    fn fd_fdstat_get(_: i32, _: i32) -> i32 {
        unimplemented!("fd_fdstat_get is not implemented")
    }

    fn fd_close(_: i32) -> i32 {
        unimplemented!("fd_close is not implemented")
    }

    fn environ_sizes_get(_: i32, _: i32) -> i32 {
        unimplemented!("environ_sizes_get is not implemented")
    }

    fn environ_get(_: i32, _: i32) -> i32 {
        unimplemented!("environ_get is not implemented")
    }

    fn set_data(_: i32, _: i32, _: i32) {
        unimplemented!("set_data is not implemented")
    }

    fn get_data(_: i32, _: i32) -> i32 {
        unimplemented!("get_data is not implemented")
    }

    fn env_load_verifier_crs() -> i32 {
        unimplemented!("env_load_verifier_crs is not implemented")
    }

    fn env_load_prover_crs(_: i32) -> i32 {
        unimplemented!("env_load_prover_crs is not implemented")
    }
}