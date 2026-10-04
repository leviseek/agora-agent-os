;; agentos wasm capability example: an uncooperative guest.
;;
;; It loops forever on purpose. The runtime must stop it with the epoch watchdog and report a
;; timeout - that is the whole point of running untrusted code in a sandbox.
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))

  (func (export "alloc") (param $len i32) (result i32)
    (global.get $heap))

  (func (export "invoke") (param $ptr i32) (param $len i32) (result i64)
    (loop $forever
      (br $forever))
    (i64.const 0))
)
