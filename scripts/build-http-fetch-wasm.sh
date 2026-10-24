#!/usr/bin/env bash
# Rebuild the built-in http_fetch WASM module from wasm-modules/http-fetch and copy it to
# crates/temper-wasm/modules/http_fetch.wasm, the binary temper-server embeds.
#
# The toolchain is the one rust-toolchain.toml pins, and the build is --locked. Cargo derives each
# package's build hash, which names its symbols and orders its functions in the output, from the
# package's path: relative to the workspace root for a package inside it, absolute otherwise. The
# SDK sits outside the module's own workspace, so the build copies both into one staging workspace,
# where both paths are relative. Source paths in panic strings are remapped. A build therefore
# matches wherever the checkout sits, and CI runs --check.
#
#   scripts/build-http-fetch-wasm.sh          rebuild and replace the embedded binary
#   scripts/build-http-fetch-wasm.sh --check  rebuild and compare with the embedded binary
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
module_dir="$root/wasm-modules/http-fetch"
embedded="$root/crates/temper-wasm/modules/http_fetch.wasm"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
stage="$(cd "$stage" && pwd -P)"
mkdir -p "$stage/crates" "$stage/wasm-modules/http-fetch"
cp "$root/rust-toolchain.toml" "$stage/"
cp -R "$root/crates/temper-wasm-sdk" "$stage/crates/"
cp -R "$module_dir/src" "$stage/wasm-modules/http-fetch/"
cp "$module_dir/Cargo.lock" "$stage/"
# The staging root is the module's workspace here, so its own [workspace] table is dropped.
grep -vx '\[workspace\]' "$module_dir/Cargo.toml" > "$stage/wasm-modules/http-fetch/Cargo.toml"
printf '[workspace]\nmembers = ["wasm-modules/http-fetch"]\nresolver = "3"\n' > "$stage/Cargo.toml"

# The toolchain's own directory names the host it runs on; remapped, the build reads the same on every host.
sysroot="$(cd "$stage" && rustc --print sysroot)"
export RUSTFLAGS="--remap-path-prefix=$stage=/temper --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$sysroot=/rust-sysroot"
export CARGO_TARGET_DIR="$stage/target"

(cd "$stage" && cargo build --locked --release --target wasm32-unknown-unknown)
built="$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/http_fetch_module.wasm"

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
