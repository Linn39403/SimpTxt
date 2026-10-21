#!/usr/bin/env bash
# Captures the real production egui tree without a window server.
# Uses the caller's Rust toolchain and installed graphics drivers.
set -euo pipefail
cd "$(dirname "$0")/.."
export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
export MESA_SHADER_CACHE_DISABLE="${MESA_SHADER_CACHE_DISABLE:-true}"
if [[ -z "${XDG_RUNTIME_DIR:-}" ]]; then
  XDG_RUNTIME_DIR=$(mktemp -d "${TMPDIR:-/tmp}/notepad-prototype-runtime.XXXXXX")
  export XDG_RUNTIME_DIR
  trap 'rm -rf "$XDG_RUNTIME_DIR"' EXIT
fi
mkdir -p artifacts
cargo run --locked --features headless-capture -- --screenshot artifacts/light.png examples/welcome.txt
cargo run --locked --features headless-capture -- --screenshot artifacts/dark.png examples/sample.rs --dark
cargo run --locked --features headless-capture -- --screenshot artifacts/settings.png examples/welcome.txt --settings
