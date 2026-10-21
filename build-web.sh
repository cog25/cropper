#!/usr/bin/env sh
# Web build with wasm threads (see .cargo/config.toml); toolchain comes from
# mise.toml (nightly). Usage: ./build-web.sh [serve|build] [extra trunk args]
set -e
cd "$(dirname "$0")/crates/cropper-gui"
export CARGO_UNSTABLE_BUILD_STD=std,panic_abort
cmd="${1:-build}"; [ $# -gt 0 ] && shift
exec trunk "$cmd" "$@"
