# Compiled identity regression fixture

`identity.wasm` is built from `src/lib.rs` using the workspace WASM SDK.
The normal `wasm_identity_admission` integration test loads this binary and runs
it through the production WASM dispatcher, OData handlers, Cedar and local libSQL.
It performs no external network calls. Only fake secret values are used.

Rebuild from the kernel repository root:

```sh
RUSTFLAGS='-C link-arg=--allow-undefined' cargo build --release --target wasm32-unknown-unknown --manifest-path crates/temper-server/tests/fixtures/wasm-identity/Cargo.toml
cp crates/temper-server/tests/fixtures/wasm-identity/target/wasm32-unknown-unknown/release/temper_wasm_identity_fixture.wasm crates/temper-server/tests/fixtures/wasm-identity/identity.wasm
cargo test -p temper-server --features observe --test wasm_identity_admission
```

Before the fix: the caller-admission and local module-identity tests fail; the
shared-secret test passes. After the fix: all five tests pass, including denied
ownership, repeated-request state checks and malformed configuration.
