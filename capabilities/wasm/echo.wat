;; agentos wasm capability example: JSON echo through the sandbox.
;;
;; ABI (agentos.wasm.v1):
;;   alloc(len: i32) -> i32          reserve len bytes, return offset
;;   invoke(ptr: i32, len: i32) -> i64
;;                                   handle the JSON request, return
;;                                   (result_ptr << 32) | result_len
;;   memory                          exported linear memory
;;
;; The guest copies the request bytes into a fresh buffer and returns them unchanged, and calls
;; host_log once so the host-function surface is exercised. This is the smallest thing that can
;; be loaded, instantiated, timed out and permission-checked.
(module
  (import "agentos" "host_log" (func $host_log (param i32 i32 i32) (result i32)))

  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 2048))

  ;; "wasm-echo invoked" lives at offset 16, length 17
  (data (i32.const 16) "wasm-echo invoked")

  (func $alloc (export "alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap
      (i32.add (global.get $heap) (i32.add (local.get $len) (i32.const 16))))
    (local.get $p))

  (func (export "invoke") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    ;; host_log(level = 0, ptr = 16, len = 17); a denied call would return -1 and be ignored
    (drop (call $host_log (i32.const 0) (i32.const 16) (i32.const 17)))
    (local.set $out (call $alloc (local.get $len)))
    (memory.copy (local.get $out) (local.get $ptr) (local.get $len))
    ;; return (out << 32) | len
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (local.get $len))))
)
