#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$SCRIPT_DIR"

# 1. Ensure PATH contains cargo and mingw
export PATH="$HOME/.cargo/bin:/home/linuxbrew/.linuxbrew/bin:$PATH"

echo "[1/3] Building Sampleman for Windows (x86_64-pc-windows-gnu)..."
cargo build --release --target x86_64-pc-windows-gnu

# 2. Define install destination (Windows AppData Local Programs)
WIN_USER="${WIN_USER:-ykuwa}"
INSTALL_DIR_WSL="/mnt/c/Users/${WIN_USER}/AppData/Local/Programs/Sampleman"
INSTALL_DIR_WIN="C:\\Users\\${WIN_USER}\\AppData\\Local\\Programs\\Sampleman"

echo "[2/3] Installing to ${INSTALL_DIR_WIN}..."
mkdir -p "${INSTALL_DIR_WSL}"
cp target/x86_64-pc-windows-gnu/release/sampleman.exe "${INSTALL_DIR_WSL}/sampleman.exe"
if [ -f "soundfont.sf2" ]; then
    cp soundfont.sf2 "${INSTALL_DIR_WSL}/soundfont.sf2"
fi

echo "[3/3] Launching Sampleman on Windows..."
nohup cmd.exe /c "start /d \"${INSTALL_DIR_WIN}\" sampleman.exe" </dev/null >/dev/null 2>&1 &

echo "Done! Sampleman installed and launched successfully."
