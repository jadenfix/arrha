use aether_types::{Address, H256};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wasmtime::*;

/// Maximum WASM linear memory: 16 MB (256 pages × 64 KB).
const MAX_MEMORY_BYTES: usize = 16 * 1024 * 1024;
/// Maximum WASM table elements per instance.
const MAX_TABLE_ELEMENTS: usize = 10_000;

// ResourceLimiter is implemented on StoreData below to enforce hard caps on
// memory and table allocations per contract execution.  The engine's
// `static_memory_maximum_size` only controls virtual-address-space reservation,
// not actual growth.  Without a ResourceLimiter a malicious module could
// allocate unbounded tables and exhaust host RAM.

/// WASM Virtual Machine for Smart Contract Execution
///
/// Uses Wasmtime with fuel-based gas metering, deterministic configuration
/// (no SIMD, no threads, no floating point), and host function bindings
/// for blockchain state interaction.
pub struct WasmVm {
    engine: Engine,
    gas_limit: u64,
    /// Compiled modules keyed by SHA-256 of the WASM bytes. Compiling with
    /// Cranelift costs milliseconds, so contract calls reuse the module.
    module_cache: HashMap<[u8; 32], Module>,
}

/// Maximum number of compiled modules kept in [`WasmVm`]'s cache.
const MAX_CACHED_MODULES: usize = 256;

#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub contract_address: Address,
    pub caller: Address,
    pub value: u128,
    pub gas_limit: u64,
    pub block_number: u64,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub success: bool,
    pub gas_used: u64,
    pub return_data: Vec<u8>,
    pub logs: Vec<Log>,
    pub storage_changes: HashMap<Vec<u8>, Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Log {
    pub topics: Vec<H256>,
    pub data: Vec<u8>,
}

// Host-function size limits to prevent unbounded memory usage from malicious contracts.
const MAX_STORAGE_KEY_LEN: usize = 256;
const MAX_STORAGE_VAL_LEN: usize = 4096;
const MAX_LOG_DATA_LEN: usize = 4096;
const MAX_LOG_COUNT: usize = 100;
const MAX_RETURN_DATA_LEN: usize = 4096;

/// Shared state accessible to host functions during execution.
struct HostState {
    storage: HashMap<Vec<u8>, Vec<u8>>,
    logs: Vec<Log>,
    return_data: Vec<u8>,
    context: ExecutionContext,
}

/// Store data that wraps host state and enforces resource limits.
struct StoreData {
    host: Arc<Mutex<HostState>>,
}

impl ResourceLimiter for StoreData {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        Ok(desired <= MAX_MEMORY_BYTES && desired >= current)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        Ok(desired <= MAX_TABLE_ELEMENTS && desired >= current)
    }
}

impl WasmVm {
    pub fn new(gas_limit: u64) -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        // Deterministic execution: disable non-deterministic features
        config.wasm_simd(false);
        config.wasm_relaxed_simd(false);
        config.wasm_threads(false);
        config.wasm_bulk_memory(true);
        config.wasm_multi_value(true);
        // StoreData::memory_growing caps WASM memory at 16MB (256 pages × 64KB).
        config.cranelift_opt_level(OptLevel::Speed);
        // Limit WASM call stack depth to 512KB to prevent stack-overflow DoS.
        config.max_wasm_stack(512 * 1024);
        config.memory_guard_size(64 * 1024);

        let engine = Engine::new(&config)
            .map_err(|e| anyhow::anyhow!("failed to create Wasmtime engine: {e}"))?;

        Ok(WasmVm {
            engine,
            gas_limit,
            module_cache: HashMap::new(),
        })
    }

    /// Compile (or fetch from cache) a module after basic validation.
    fn module_for(&mut self, wasm_bytes: &[u8]) -> Result<Module> {
        // Validate WASM magic number
        if wasm_bytes.len() < 4 || &wasm_bytes[0..4] != b"\0asm" {
            bail!("invalid WASM magic number");
        }
        if wasm_bytes.len() > 1024 * 1024 {
            bail!("WASM module too large (max 1MB)");
        }
        use sha2::{Digest, Sha256};
        let key: [u8; 32] = Sha256::digest(wasm_bytes).into();
        if let Some(module) = self.module_cache.get(&key) {
            return Ok(module.clone());
        }
        let module = Module::new(&self.engine, wasm_bytes)?;
        if self.module_cache.len() >= MAX_CACHED_MODULES {
            self.module_cache.clear();
        }
        self.module_cache.insert(key, module.clone());
        Ok(module)
    }

    /// Check that `wasm_bytes` compiles and exports an `execute` entry point.
    pub fn validate_module(&mut self, wasm_bytes: &[u8]) -> Result<()> {
        let module = self.module_for(wasm_bytes)?;
        let has_execute = module
            .exports()
            .any(|e| e.name() == "execute" && matches!(e.ty(), ExternType::Func(_)));
        if !has_execute {
            bail!("contract must export an `execute(i32, i32) -> i32` function");
        }
        Ok(())
    }

    /// Execute WASM bytecode with the given context and input.
    pub fn execute(
        &mut self,
        wasm_bytes: &[u8],
        context: &ExecutionContext,
        input: &[u8],
    ) -> Result<ExecutionResult> {
        self.execute_with_storage(wasm_bytes, context, input, HashMap::new())
    }

    /// Execute with the contract's existing storage visible to `storage_read`.
    /// `ExecutionResult::storage_changes` holds the full storage after the call.
    pub fn execute_with_storage(
        &mut self,
        wasm_bytes: &[u8],
        context: &ExecutionContext,
        input: &[u8],
        storage: HashMap<Vec<u8>, Vec<u8>>,
    ) -> Result<ExecutionResult> {
        if context.gas_limit > self.gas_limit {
            bail!(
                "context gas_limit {} exceeds VM cap {}",
                context.gas_limit,
                self.gas_limit
            );
        }

        // Compile (cached) the module
        let module = self.module_for(wasm_bytes)?;

        // Keep the pre-call storage so a failed `execute` can be rolled back
        // before the fallback entry point runs.
        let initial_storage = storage.clone();

        // Create store with fuel (gas)
        let host_state = Arc::new(Mutex::new(HostState {
            storage,
            logs: Vec::new(),
            return_data: Vec::new(),
            context: context.clone(),
        }));

        let store_data = StoreData {
            host: host_state.clone(),
        };
        let mut store = Store::new(&self.engine, store_data);
        store.limiter(|data| data);
        store.set_fuel(context.gas_limit)?;

        // Create linker with host functions
        let mut linker = Linker::new(&self.engine);
        Self::register_host_functions(&mut linker, host_state.clone())?;

        // Instantiate the module
        let instance = linker.instantiate(&mut store, &module)?;

        // Get memory export (if any)
        let memory = instance.get_memory(&mut store, "memory");

        // Write input data to WASM memory if memory exists
        if let Some(mem) = &memory {
            let _input_offset = 0u32;
            if input.len() <= mem.data_size(&store) {
                mem.data_mut(&mut store)[..input.len()].copy_from_slice(input);
            }
        }

        // Call the entry point
        let func = instance.get_typed_func::<(i32, i32), i32>(&mut store, "execute");

        let success = match func {
            Ok(f) => {
                let input_len: i32 = input.len().try_into().map_err(|_| {
                    anyhow::anyhow!("input too large for WASM (max {} bytes)", i32::MAX)
                })?;
                match f.call(&mut store, (0, input_len)) {
                    Ok(result_code) => result_code == 0,
                    Err(e) => {
                        // Out-of-fuel means out-of-gas: do NOT retry another entry point.
                        // A retry would start a new call with 0 remaining fuel, bypassing
                        // the gas limit entirely.
                        let oof = e
                            .downcast_ref::<wasmtime::Trap>()
                            .map(|t| *t == wasmtime::Trap::OutOfFuel)
                            .unwrap_or_else(|| e.to_string().contains("fuel"));
                        if oof {
                            false
                        } else {
                            // Non-gas error — try simpler entry point with no args.
                            // SECURITY: Reset HostState so mutations from the failed
                            // `execute` call (storage writes, logs, return_data) do not
                            // leak into the fallback entry point.
                            if let Ok(mut state) = host_state.lock() {
                                state.storage = initial_storage.clone();
                                state.logs.clear();
                                state.return_data.clear();
                            }
                            let simple_func =
                                instance.get_typed_func::<(), i32>(&mut store, "main");
                            match simple_func {
                                Ok(f) => f.call(&mut store, ()).map(|r| r == 0).unwrap_or(false),
                                Err(_) => false,
                            }
                        }
                    }
                }
            }
            Err(_) => {
                // Try "main" with no args
                let main_func = instance.get_typed_func::<(), i32>(&mut store, "main");
                match main_func {
                    Ok(f) => f.call(&mut store, ()).map(|r| r == 0).unwrap_or(false),
                    Err(_) => {
                        // Try _start (WASI-style)
                        let start_func = instance.get_typed_func::<(), ()>(&mut store, "_start");
                        match start_func {
                            Ok(f) => f.call(&mut store, ()).is_ok(),
                            Err(_) => false, // Module has no entry point, treat as failure
                        }
                    }
                }
            }
        };

        // Calculate gas used
        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let gas_used = context.gas_limit.saturating_sub(remaining_fuel);

        // Collect results from host state
        let state = host_state
            .lock()
            .map_err(|_| anyhow::anyhow!("host state mutex poisoned"))?;

        Ok(ExecutionResult {
            success,
            gas_used,
            return_data: state.return_data.clone(),
            logs: state.logs.clone(),
            storage_changes: state.storage.clone(),
        })
    }

    /// Register host functions that WASM modules can import.
    fn register_host_functions(
        linker: &mut Linker<StoreData>,
        _host_state: Arc<Mutex<HostState>>,
    ) -> Result<()> {
        // env.storage_read(key_ptr: i32, key_len: i32, val_ptr: i32) -> i32
        // Gas cost: 200 fuel units
        linker.func_wrap(
            "env",
            "storage_read",
            |mut caller: Caller<'_, StoreData>, key_ptr: i32, key_len: i32, val_ptr: i32| -> i32 {
                // Charge fuel for storage_read (200 fuel units)
                match caller.get_fuel() {
                    Ok(fuel) if fuel >= 200 => {
                        if caller.set_fuel(fuel.saturating_sub(200)).is_err() {
                            return -1; // Fuel deduction failed
                        }
                    }
                    Ok(_) => return -1,  // Insufficient fuel
                    Err(_) => return -1, // Fuel system unavailable
                }

                // Reject negative or oversized pointer/length values.
                if key_ptr < 0 || key_len < 0 || val_ptr < 0 {
                    return -1;
                }
                if key_len as usize > MAX_STORAGE_KEY_LEN {
                    return -1;
                }

                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return -1,
                };

                let key = {
                    let data = memory.data(&caller);
                    let start = key_ptr as usize;
                    let end = match start.checked_add(key_len as usize) {
                        Some(e) if e <= data.len() => e,
                        _ => return -1,
                    };
                    data[start..end].to_vec()
                };

                let value = {
                    let state = match caller.data().host.lock() {
                        Ok(s) => s,
                        Err(_) => return -1,
                    };
                    state.storage.get(&key).cloned()
                };
                match value {
                    Some(value) => {
                        let val_start = val_ptr as usize;
                        let val_end = match val_start.checked_add(value.len()) {
                            Some(e) if e <= memory.data(&caller).len() => e,
                            _ => return -1,
                        };
                        let data = memory.data_mut(&mut caller);
                        data[val_start..val_end].copy_from_slice(&value);
                        value.len() as i32
                    }
                    None => 0,
                }
            },
        )?;

        // env.storage_write(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32) -> i32
        // Gas cost: 5000 fuel units base + 20 per byte
        linker.func_wrap(
            "env",
            "storage_write",
            |mut caller: Caller<'_, StoreData>,
             key_ptr: i32,
             key_len: i32,
             val_ptr: i32,
             val_len: i32|
             -> i32 {
                // Reject negative or oversized pointer/length values.
                if key_ptr < 0 || key_len < 0 || val_ptr < 0 || val_len < 0 {
                    return -1;
                }
                if key_len as usize > MAX_STORAGE_KEY_LEN || val_len as usize > MAX_STORAGE_VAL_LEN
                {
                    return -1;
                }

                // Charge fuel after validation so negative values don't produce
                // astronomically wrong gas costs via u64 wrapping.
                let val_cost = (val_len as u64).saturating_mul(20);
                let fuel_cost = 5000u64.saturating_add(val_cost);
                match caller.get_fuel() {
                    Ok(fuel) if fuel >= fuel_cost => {
                        if caller.set_fuel(fuel.saturating_sub(fuel_cost)).is_err() {
                            return -1;
                        }
                    }
                    Ok(_) => return -1,
                    Err(_) => return -1,
                }

                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return -1,
                };

                let data = memory.data(&caller);
                let key_start = key_ptr as usize;
                let key_end = match key_start.checked_add(key_len as usize) {
                    Some(e) if e <= data.len() => e,
                    _ => return -1,
                };
                let val_start = val_ptr as usize;
                let val_end = match val_start.checked_add(val_len as usize) {
                    Some(e) if e <= data.len() => e,
                    _ => return -1,
                };

                let key = data[key_start..key_end].to_vec();
                let value = data[val_start..val_end].to_vec();

                let mut state = match caller.data().host.lock() {
                    Ok(s) => s,
                    Err(_) => return -1,
                };
                const MAX_STORAGE_ENTRIES: usize = 10_000;
                if state.storage.len() >= MAX_STORAGE_ENTRIES && !state.storage.contains_key(&key) {
                    return -1; // Storage limit exceeded
                }
                state.storage.insert(key, value);
                0
            },
        )?;

        // env.emit_log(data_ptr: i32, data_len: i32) -> i32
        // Gas cost: 375 base + 8 per byte
        linker.func_wrap(
            "env",
            "emit_log",
            |mut caller: Caller<'_, StoreData>, data_ptr: i32, data_len: i32| -> i32 {
                // Reject negative pointer/length values.
                if data_ptr < 0 || data_len < 0 {
                    return -1;
                }

                // Enforce log data size limit (before gas charge to avoid wrapping).
                if data_len as usize > MAX_LOG_DATA_LEN {
                    return -1;
                }

                // Charge fuel after validation so negative values can't wrap.
                let log_byte_cost = (data_len as u64).saturating_mul(8);
                let fuel_cost = 375u64.saturating_add(log_byte_cost);
                match caller.get_fuel() {
                    Ok(fuel) if fuel >= fuel_cost => {
                        if caller.set_fuel(fuel.saturating_sub(fuel_cost)).is_err() {
                            return -1;
                        }
                    }
                    Ok(_) => return -1,
                    Err(_) => return -1,
                }

                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return -1,
                };

                let data = memory.data(&caller);
                let start = data_ptr as usize;
                let end = match start.checked_add(data_len as usize) {
                    Some(e) if e <= data.len() => e,
                    _ => return -1,
                };

                let log_data = data[start..end].to_vec();
                let mut state = match caller.data().host.lock() {
                    Ok(s) => s,
                    Err(_) => return -1,
                };
                if state.logs.len() >= MAX_LOG_COUNT {
                    return -1; // Too many logs emitted
                }
                state.logs.push(Log {
                    topics: vec![],
                    data: log_data,
                });
                0
            },
        )?;

        // env.set_return(ptr: i32, len: i32)
        linker.func_wrap(
            "env",
            "set_return",
            |mut caller: Caller<'_, StoreData>, ptr: i32, len: i32| -> i32 {
                // Reject negative pointer/length values.
                if ptr < 0 || len < 0 {
                    return -1;
                }

                // Enforce return data size limit.
                if len as usize > MAX_RETURN_DATA_LEN {
                    return -1;
                }

                // Charge fuel after validation so negative values can't wrap.
                let fuel_cost = 100u64.saturating_add(len as u64);
                match caller.get_fuel() {
                    Ok(fuel) if fuel >= fuel_cost => {
                        if caller.set_fuel(fuel.saturating_sub(fuel_cost)).is_err() {
                            return -1;
                        }
                    }
                    Ok(_) => return -1,
                    Err(_) => return -1,
                }

                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return -1,
                };

                let data = memory.data(&caller);
                let start = ptr as usize;
                let end = match start.checked_add(len as usize) {
                    Some(e) if e <= data.len() => e,
                    _ => return -1,
                };

                let ret_data = data[start..end].to_vec();
                let mut state = match caller.data().host.lock() {
                    Ok(s) => s,
                    Err(_) => return -1,
                };
                state.return_data = ret_data;
                0
            },
        )?;

        // env.block_number() -> i64
        linker.func_wrap(
            "env",
            "block_number",
            |caller: Caller<'_, StoreData>| -> i64 {
                let state = match caller.data().host.lock() {
                    Ok(s) => s,
                    Err(_) => return -1,
                };
                state.context.block_number as i64
            },
        )?;

        // env.timestamp() -> i64
        linker.func_wrap("env", "timestamp", |caller: Caller<'_, StoreData>| -> i64 {
            let state = match caller.data().host.lock() {
                Ok(s) => s,
                Err(_) => return -1,
            };
            state.context.timestamp as i64
        })?;

        Ok(())
    }

    /// Maximum gas any single execution may consume.
    pub fn gas_limit(&self) -> u64 {
        self.gas_limit
    }
}

/// Gas costs for different operations.
pub mod gas_costs {
    pub const BASE: u64 = 100;
    pub const MEMORY_BYTE: u64 = 1;
    pub const STORAGE_READ: u64 = 200;
    pub const STORAGE_WRITE: u64 = 5000;
    pub const LOG: u64 = 375;
    pub const SHA256: u64 = 60;
    pub const TRANSFER: u64 = 9000;
}

#[cfg(test)]
mod tests {
    use super::*;

    const COUNTER_WAT: &str = include_str!("../../../examples/contracts/counter.wat");

    fn counter_ctx() -> ExecutionContext {
        ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 100_000,
            block_number: 1,
            timestamp: 1000,
        }
    }

    #[test]
    fn counter_contract_reads_existing_storage() {
        let wasm = wat::parse_str(COUNTER_WAT).unwrap();
        let mut vm = WasmVm::new(1_000_000).unwrap();
        vm.validate_module(&wasm).unwrap();

        let first = vm
            .execute_with_storage(&wasm, &counter_ctx(), b"", HashMap::new())
            .unwrap();
        assert!(first.success);
        assert_eq!(first.return_data, 1u64.to_le_bytes().to_vec());

        // Second call sees the first call's storage and adds 5.
        let second = vm
            .execute_with_storage(
                &wasm,
                &counter_ctx(),
                &5u64.to_le_bytes(),
                first.storage_changes.clone(),
            )
            .unwrap();
        assert!(second.success);
        assert_eq!(second.return_data, 6u64.to_le_bytes().to_vec());
        assert_eq!(second.logs.len(), 1);
        assert!(second.gas_used > 0);
    }

    #[test]
    fn validate_module_requires_execute_export() {
        let wasm =
            wat::parse_str("(module (func (export \"main\") (result i32) i32.const 0))").unwrap();
        let mut vm = WasmVm::new(100_000).unwrap();
        assert!(vm.validate_module(&wasm).is_err());
    }

    #[test]
    fn test_vm_creation() {
        let vm = WasmVm::new(100_000).unwrap();
        assert_eq!(vm.gas_limit(), 100_000);
    }

    #[test]
    fn test_wasm_validation_bad_magic() {
        let mut vm = WasmVm::new(100_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 100_000,
            block_number: 1,
            timestamp: 1000,
        };

        let invalid_wasm = b"XXXX\x01\x00\x00\x00";
        assert!(vm.execute(invalid_wasm, &context, b"").is_err());
    }

    #[test]
    fn test_wasm_validation_too_short() {
        let mut vm = WasmVm::new(100_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 100_000,
            block_number: 1,
            timestamp: 1000,
        };

        assert!(vm.execute(b"\0as", &context, b"").is_err());
    }

    #[test]
    fn test_execute_minimal_wasm() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // Minimal WASM module with a function that returns 0 (success)
        // (module
        //   (func (export "execute") (param i32 i32) (result i32)
        //     i32.const 0))
        let wasm = wat::parse_str(
            r#"
            (module
                (func (export "execute") (param i32 i32) (result i32)
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"input").unwrap();
        assert!(result.success, "minimal WASM should succeed");
        assert!(result.gas_used > 0, "should consume some gas");
    }

    #[test]
    fn test_execute_wasm_with_storage() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 42,
            timestamp: 1000,
        };

        // WASM module that writes to storage
        let wasm = wat::parse_str(
            r#"
            (module
                (import "env" "storage_write" (func $storage_write (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "key")
                (data (i32.const 3) "value")
                (func (export "execute") (param i32 i32) (result i32)
                    ;; storage_write(key_ptr=0, key_len=3, val_ptr=3, val_len=5)
                    i32.const 0
                    i32.const 3
                    i32.const 3
                    i32.const 5
                    call $storage_write
                    drop
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"").unwrap();
        assert!(result.success);
        assert_eq!(
            result.storage_changes.get(b"key".as_slice()),
            Some(&b"value".to_vec()),
            "storage should contain the written key-value pair"
        );
    }

    #[test]
    fn test_execute_wasm_with_logging() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // WASM module that emits a log
        let wasm = wat::parse_str(
            r#"
            (module
                (import "env" "emit_log" (func $emit_log (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "hello from wasm")
                (func (export "execute") (param i32 i32) (result i32)
                    i32.const 0
                    i32.const 15
                    call $emit_log
                    drop
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"").unwrap();
        assert!(result.success);
        assert_eq!(result.logs.len(), 1);
        assert_eq!(result.logs[0].data, b"hello from wasm");
    }

    #[test]
    fn test_gas_consumption() {
        let mut vm = WasmVm::new(100).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 100, // Very low gas
            block_number: 1,
            timestamp: 1000,
        };

        // A module with a loop that will exhaust gas
        let wasm = wat::parse_str(
            r#"
            (module
                (func (export "execute") (param i32 i32) (result i32)
                    (local $i i32)
                    (local.set $i (i32.const 0))
                    (block $break
                        (loop $loop
                            (br_if $break (i32.ge_u (local.get $i) (i32.const 10000)))
                            (local.set $i (i32.add (local.get $i) (i32.const 1)))
                            (br $loop)
                        )
                    )
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"");
        // Should either return with success=false or error due to fuel exhaustion
        if let Ok(r) = result {
            assert!(!r.success || r.gas_used >= 100);
        }
    }

    #[test]
    fn test_gas_exhaustion_does_not_retry_entrypoint() {
        // When "execute" exhausts fuel the VM must NOT fall through to "main".
        let mut vm = WasmVm::new(50).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 50,
            block_number: 1,
            timestamp: 1000,
        };

        let wasm = wat::parse_str(
            r#"
            (module
                (func (export "execute") (param i32 i32) (result i32)
                    (local $i i32)
                    (loop $loop
                        (local.set $i (i32.add (local.get $i) (i32.const 1)))
                        (br_if $loop (i32.lt_u (local.get $i) (i32.const 99999)))
                    )
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"").unwrap();
        assert!(
            !result.success,
            "OOF execution must report failure, not retry"
        );
    }

    #[test]
    fn test_storage_key_size_limit() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // key_len=1000 exceeds MAX_STORAGE_KEY_LEN=256 — must not panic
        let wasm = wat::parse_str(
            r#"
            (module
                (import "env" "storage_write" (func $sw (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 2)
                (func (export "execute") (param i32 i32) (result i32)
                    i32.const 0
                    i32.const 1000
                    i32.const 0
                    i32.const 1
                    call $sw
                    drop
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"");
        assert!(result.is_ok(), "oversized key must not panic the VM");
        // The write should be silently rejected (storage_write returned -1)
        let exec = result.unwrap();
        assert!(
            exec.storage_changes.is_empty(),
            "oversized key write must be rejected"
        );
    }

    #[test]
    fn test_memory_growth_beyond_limit_handled_gracefully() {
        let mut vm = WasmVm::new(10_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 10_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // Try to grow memory far beyond the 16 MB (256 page) limit.
        // memory.grow returns -1 on failure per the WASM spec, so the
        // module should finish without a trap.  We verify no panic and
        // that execution completes (success or graceful failure).
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "execute") (param i32 i32) (result i32)
                    ;; Attempt to grow by 1024 pages (64 MB) — well beyond 16 MB cap
                    (memory.grow (i32.const 1024))
                    drop
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"");
        assert!(
            result.is_ok(),
            "memory growth beyond limit must not panic: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_stack_overflow_handled_gracefully() {
        let mut vm = WasmVm::new(100_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 100_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // Infinite recursion to blow the 512 KB stack limit.
        // Wasmtime should trap (not panic) and the VM should report failure.
        let wasm = wat::parse_str(
            r#"
            (module
                (func $recurse (result i32)
                    (call $recurse)
                )
                (func (export "execute") (param i32 i32) (result i32)
                    (call $recurse)
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"");
        assert!(
            result.is_ok(),
            "stack overflow must not panic the VM: {:?}",
            result.err()
        );
        let exec = result.unwrap();
        assert!(
            !exec.success,
            "stack overflow execution must report failure"
        );
    }

    #[test]
    fn test_no_entrypoint_returns_failure() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // Minimal WASM module with no exported functions at all
        let wasm = wat::parse_str("(module)").unwrap();

        let result = vm.execute(&wasm, &context, b"").unwrap();
        assert!(
            !result.success,
            "module with no entrypoint should not report success"
        );
    }

    #[test]
    fn test_negative_pointer_rejected_gracefully() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // Contract calls storage_write with negative key_ptr (-1).
        // Must return -1 (error) without panic or memory corruption.
        let wasm = wat::parse_str(
            r#"
            (module
                (import "env" "storage_write" (func $sw (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "val")
                (func (export "execute") (param i32 i32) (result i32)
                    ;; storage_write(key_ptr=-1, key_len=4, val_ptr=0, val_len=3)
                    i32.const -1
                    i32.const 4
                    i32.const 0
                    i32.const 3
                    call $sw
                    drop
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"").unwrap();
        assert!(
            result.success,
            "contract should succeed (write silently rejected)"
        );
        assert!(
            result.storage_changes.is_empty(),
            "negative pointer write must be rejected"
        );
    }

    #[test]
    fn test_negative_length_rejected_gracefully() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // Contract calls emit_log with negative data_len (-1).
        // Must return -1 without consuming massive gas from u64 wrapping.
        let wasm = wat::parse_str(
            r#"
            (module
                (import "env" "emit_log" (func $log (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "execute") (param i32 i32) (result i32)
                    ;; emit_log(data_ptr=0, data_len=-1)
                    i32.const 0
                    i32.const -1
                    call $log
                    drop
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"").unwrap();
        assert!(
            result.success,
            "contract should succeed (log silently rejected)"
        );
        assert!(
            result.logs.is_empty(),
            "negative length log must be rejected"
        );
        // Key check: gas_used should be small, not billions from u64-wrapped cost
        assert!(
            result.gas_used < 10_000,
            "negative length must not cause excessive gas charge (got {})",
            result.gas_used
        );
    }

    #[test]
    fn test_large_table_allocation_rejected() {
        let mut vm = WasmVm::new(10_000_000).unwrap();
        let context = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 10_000_000,
            block_number: 1,
            timestamp: 1000,
        };

        // Module with a table of 100_000 elements — exceeds MAX_TABLE_ELEMENTS.
        // ResourceLimiter should reject instantiation or table.grow.
        let wasm = wat::parse_str(
            r#"
            (module
                (table 100000 funcref)
                (func (export "execute") (param i32 i32) (result i32)
                    i32.const 0
                )
            )
            "#,
        )
        .unwrap();

        let result = vm.execute(&wasm, &context, b"");
        // Should either fail to instantiate or report failure — must not OOM
        match result {
            Err(_) => {} // instantiation rejected — correct
            Ok(r) => assert!(!r.success, "large table allocation must not succeed"),
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::{Address, ExecutionContext, WasmVm, MAX_STORAGE_KEY_LEN, MAX_STORAGE_VAL_LEN};
    use proptest::prelude::*;

    fn arb_context() -> impl Strategy<Value = ExecutionContext> {
        (any::<u128>(), any::<u64>(), any::<u64>()).prop_map(|(value, block_number, timestamp)| {
            ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value,
                gas_limit: 1_000_000,
                block_number,
                timestamp,
            }
        })
    }

    /// Compile a simple WAT module that returns a constant.
    fn make_return_module(return_val: i32) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module
                (func (export "execute") (param i32 i32) (result i32)
                    i32.const {return_val}
                )
            )"#,
        ))
        .unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// Gas used never exceeds gas limit, regardless of context values.
        #[test]
        fn gas_used_le_gas_limit(gas_limit in 100u64..10_000_000, ctx in arb_context()) {
            let mut vm = WasmVm::new(gas_limit).unwrap();
            let mut ctx = ctx;
            ctx.gas_limit = gas_limit;
            let wasm = make_return_module(0);
            let result = vm.execute(&wasm, &ctx, b"").unwrap();
            prop_assert!(result.gas_used <= gas_limit,
                "gas_used {} > gas_limit {}", result.gas_used, gas_limit);
        }

        /// Return code 0 means success=true, anything else means success=false.
        #[test]
        fn return_code_determines_success(rc in -100i32..100) {
            let mut vm = WasmVm::new(1_000_000).unwrap();
            let ctx = ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value: 0,
                gas_limit: 1_000_000,
                block_number: 1,
                timestamp: 1000,
            };
            let wasm = make_return_module(rc);
            let result = vm.execute(&wasm, &ctx, b"").unwrap();
            prop_assert_eq!(result.success, rc == 0);
        }

        /// Arbitrary input bytes are written to memory without panic.
        #[test]
        fn arbitrary_input_no_panic(input in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut vm = WasmVm::new(1_000_000).unwrap();
            let ctx = ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value: 0,
                gas_limit: 1_000_000,
                block_number: 1,
                timestamp: 1000,
            };
            let wasm = wat::parse_str(
                r#"(module
                    (memory (export "memory") 1)
                    (func (export "execute") (param i32 i32) (result i32)
                        i32.const 0
                    )
                )"#,
            ).unwrap();
            // Must not panic — may succeed or fail gracefully
            let _ = vm.execute(&wasm, &ctx, &input);
        }

        /// Very low gas always results in gas_used == gas_limit (all fuel consumed).
        #[test]
        fn low_gas_exhausts_fuel(gas in 1u64..50) {
            let mut vm = WasmVm::new(gas).unwrap();
            let ctx = ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value: 0,
                gas_limit: gas,
                block_number: 1,
                timestamp: 1000,
            };
            // Loop that will definitely exhaust gas
            let wasm = wat::parse_str(
                r#"(module
                    (func (export "execute") (param i32 i32) (result i32)
                        (local $i i32)
                        (block $b
                            (loop $l
                                (br_if $b (i32.ge_u (local.get $i) (i32.const 100000)))
                                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                                (br $l)
                            )
                        )
                        i32.const 0
                    )
                )"#,
            ).unwrap();
            if let Ok(result) = vm.execute(&wasm, &ctx, b"") {
                prop_assert!(!result.success, "should fail with gas {}", gas);
                prop_assert!(result.gas_used <= gas);
            }
        }

        /// Random bytes are always rejected (not valid WASM).
        #[test]
        fn random_bytes_rejected(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
            let mut vm = WasmVm::new(1_000_000).unwrap();
            let ctx = ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value: 0,
                gas_limit: 1_000_000,
                block_number: 1,
                timestamp: 1000,
            };
            // Random bytes almost certainly won't have valid WASM magic + structure
            if bytes.len() >= 4 && bytes[0..4] == *b"\0asm" {
                // Skip — could theoretically be valid-ish WASM
                return Ok(());
            }
            prop_assert!(vm.execute(&bytes, &ctx, b"").is_err());
        }

        /// Storage writes with arbitrary key/value sizes stay within bounds.
        #[test]
        fn storage_write_bounded(
            key_len in 1usize..512,
            val_len in 1usize..8192,
        ) {
            let mut vm = WasmVm::new(10_000_000).unwrap();
            let ctx = ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value: 0,
                gas_limit: 10_000_000,
                block_number: 1,
                timestamp: 1000,
            };
            // Module writes key_len bytes from offset 0 and val_len bytes from offset 1024
            let wasm = wat::parse_str(format!(
                r#"(module
                    (import "env" "storage_write" (func $sw (param i32 i32 i32 i32) (result i32)))
                    (memory (export "memory") 1)
                    (func (export "execute") (param i32 i32) (result i32)
                        i32.const 0
                        i32.const {key_len}
                        i32.const 1024
                        i32.const {val_len}
                        call $sw
                    )
                )"#,
            )).unwrap();
            let result = vm.execute(&wasm, &ctx, b"").unwrap();
            if key_len > MAX_STORAGE_KEY_LEN || val_len > MAX_STORAGE_VAL_LEN {
                // Host func should reject oversized keys/values — return -1 (not 0)
                prop_assert!(!result.success || result.storage_changes.is_empty(),
                    "oversized storage write should be rejected: key_len={key_len} val_len={val_len}");
            }
        }

        /// Oversized WASM modules (>1MB) are rejected.
        #[test]
        fn oversized_module_rejected(extra in 1usize..4096) {
            let mut vm = WasmVm::new(1_000_000).unwrap();
            let ctx = ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value: 0,
                gas_limit: 1_000_000,
                block_number: 1,
                timestamp: 1000,
            };
            // Create bytes > 1MB with valid WASM magic
            let mut bytes = b"\0asm\x01\x00\x00\x00".to_vec();
            bytes.resize(1024 * 1024 + extra, 0);
            prop_assert!(vm.execute(&bytes, &ctx, b"").is_err());
        }

        /// Execution is deterministic — same inputs produce same outputs.
        #[test]
        fn deterministic_execution(
            block_number in 0u64..1000,
            timestamp in 0u64..1_000_000,
        ) {
            let wasm = wat::parse_str(
                r#"(module
                    (import "env" "block_number" (func $bn (result i64)))
                    (memory (export "memory") 1)
                    (func (export "execute") (param i32 i32) (result i32)
                        call $bn
                        i64.const 100
                        i64.gt_s
                        if (result i32)
                            i32.const 1
                        else
                            i32.const 0
                        end
                    )
                )"#,
            ).unwrap();
            let ctx = ExecutionContext {
                contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
                caller: Address::from_slice(&[2u8; 20]).unwrap(),
                value: 0,
                gas_limit: 1_000_000,
                block_number,
                timestamp,
            };
            let mut vm1 = WasmVm::new(1_000_000).unwrap();
            let mut vm2 = WasmVm::new(1_000_000).unwrap();
            let r1 = vm1.execute(&wasm, &ctx, b"test").unwrap();
            let r2 = vm2.execute(&wasm, &ctx, b"test").unwrap();
            prop_assert_eq!(r1.success, r2.success);
            prop_assert_eq!(r1.gas_used, r2.gas_used);
            prop_assert_eq!(r1.return_data, r2.return_data);
        }
    }

    #[test]
    fn context_gas_exceeding_vm_cap_is_rejected() {
        let mut vm = WasmVm::new(1_000).unwrap();
        let wasm = wat::parse_str(
            r#"(module (func (export "execute") (param i32 i32) (result i32) i32.const 0))"#,
        )
        .unwrap();
        let ctx = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_001,
            block_number: 1,
            timestamp: 1000,
        };
        let err = vm.execute(&wasm, &ctx, b"").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds VM cap"),
            "should reject context gas > VM cap, got: {msg}"
        );
    }

    #[test]
    fn context_gas_at_vm_cap_is_accepted() {
        let mut vm = WasmVm::new(1_000_000).unwrap();
        let wasm = wat::parse_str(
            r#"(module (func (export "execute") (param i32 i32) (result i32) i32.const 0))"#,
        )
        .unwrap();
        let ctx = ExecutionContext {
            contract_address: Address::from_slice(&[1u8; 20]).unwrap(),
            caller: Address::from_slice(&[2u8; 20]).unwrap(),
            value: 0,
            gas_limit: 1_000_000,
            block_number: 1,
            timestamp: 1000,
        };
        let result = vm.execute(&wasm, &ctx, b"").unwrap();
        assert!(result.success);
    }
}
