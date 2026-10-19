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
