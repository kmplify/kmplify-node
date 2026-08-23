//! Assemble the catalog's `echo` function from text and write the bytes.
//!
//!   cargo run --example emit_echo_wasm -- /path/to/echo.wasm
//!
//! The gateway serves this module and signs its sha256; the node re-hashes
//! what it downloads. Keeping the source here, next to the runtime that
//! executes it, means the fixture the node tests against and the module the
//! public fabric ships are the same bytes.
fn main() {
    let out = std::env::args().nth(1).expect("output path");
    let wasm = wat::parse_str(ECHO_WAT).expect("echo module assembles");
    std::fs::write(&out, &wasm).expect("write");
    println!("{out}: {} bytes", wasm.len());
}

const ECHO_WAT: &str = r#"
(module
  (import "wasi_snapshot_preview1" "fd_read"  (func $fd_read  (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (local $n i32)
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const 4096))
    (block $done
      (loop $again
        (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)))
        (local.set $n (i32.load (i32.const 16)))
        (br_if $done (i32.eqz (local.get $n)))
        (i32.store (i32.const 32) (i32.const 64))
        (i32.store (i32.const 36) (local.get $n))
        (drop (call $fd_write (i32.const 1) (i32.const 32) (i32.const 1) (i32.const 20)))
        (br $again))))
)"#;
