;; Counter contract for the Aether devnet.
;;
;; execute(ptr, len): input is an optional 8-byte little-endian increment
;; (default 1). Adds it to the u64 stored under key "count", writes it back,
;; emits the new value as a log and returns it.
;;
;; Host ABI (module "env"), see crates/runtime/src/vm.rs:
;;   storage_read(key_ptr, key_len, val_ptr) -> len (0 if missing, -1 on error)
;;   storage_write(key_ptr, key_len, val_ptr, val_len) -> 0 on success
;;   emit_log(ptr, len) -> 0 on success
;;   set_return(ptr, len) -> 0 on success
;; The VM copies the call input to memory offset 0.
(module
  (import "env" "storage_read" (func $storage_read (param i32 i32 i32) (result i32)))
  (import "env" "storage_write" (func $storage_write (param i32 i32 i32 i32) (result i32)))
  (import "env" "emit_log" (func $emit_log (param i32 i32) (result i32)))
  (import "env" "set_return" (func $set_return (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 1024) "count")
  (func (export "execute") (param $ptr i32) (param $len i32) (result i32)
    (local $inc i64)
    (local $cur i64)
    ;; increment = input[0..8] if provided, else 1
    (local.set $inc (i64.const 1))
    (if (i32.ge_u (local.get $len) (i32.const 8))
      (then (local.set $inc (i64.load (local.get $ptr)))))
    ;; current value (0 if the key has never been written)
    (local.set $cur (i64.const 0))
    (if (i32.eq (call $storage_read (i32.const 1024) (i32.const 5) (i32.const 2048)) (i32.const 8))
      (then (local.set $cur (i64.load (i32.const 2048)))))
    (local.set $cur (i64.add (local.get $cur) (local.get $inc)))
    (i64.store (i32.const 2048) (local.get $cur))
    (if (i32.ne (call $storage_write (i32.const 1024) (i32.const 5) (i32.const 2048) (i32.const 8)) (i32.const 0))
      (then (return (i32.const 1))))
    (drop (call $emit_log (i32.const 2048) (i32.const 8)))
    (drop (call $set_return (i32.const 2048) (i32.const 8)))
    (i32.const 0)))
