#!/usr/bin/env bash
# Rebuild the built-in http_fetch WASM module from wasm-modules/http-fetch and copy it to
# crates/temper-wasm/modules/http_fetch.wasm, the binary temper-server embeds.
#
# The toolchain is the one rust-toolchain.toml pins, and the build is --locked. Source paths are
# remapped so the binary does not carry the machine's checkout or cargo home in its panic strings.
#
#   scripts/build-http-fetch-wasm.sh          rebuild and replace the embedded binary
#   scripts/build-http-fetch-wasm.sh --check  rebuild and compare with the embedded binary
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
module_dir="$root/wasm-modules/http-fetch"
embedded="$root/crates/temper-wasm/modules/http_fetch.wasm"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
# The toolchain's own directory names the host it runs on; remapped, the build reads the same on every host.
sysroot="$(cd "$module_dir" && rustc --print sysroot)"
target_dir="$module_dir/target"

export RUSTFLAGS="--remap-path-prefix=$root=/temper --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$sysroot=/rust-sysroot"
export CARGO_TARGET_DIR="$target_dir"

(cd "$module_dir" && cargo build --locked --release --target wasm32-unknown-unknown)
built="$target_dir/wasm32-unknown-unknown/release/http_fetch_module.wasm"

if [[ "${1:-}" == "--check" ]]; then
  if cmp -s "$built" "$embedded"; then
    echo "http_fetch.wasm matches its source"
  else
    echo "http_fetch.wasm differs from a build of wasm-modules/http-fetch" >&2
    shasum -a 256 "$built" "$embedded" >&2
    exit 1
  fi
else
  cp "$built" "$embedded"
  shasum -a 256 "$embedded"
fi
