#!/usr/bin/env bash
# Project regression tests (see issue #10).
# Policy: pure-logic modules carry #[cfg(test)] unit tests
# (e.g. music_meta, midi_util). GUI event handlers and rodio
# audio-device paths are excluded (need a display / sound card).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

if command -v cargo >/dev/null 2>&1; then
    :
else
    export PATH="$HOME/.cargo/bin:$PATH"
fi

# NOTE (Linux/WSL): this needs the ALSA dev package first, e.g.
#   sudo apt-get install -y pkg-config libasound2-dev
# Windows and macOS runners already satisfy all build inputs.
cargo test --locked
