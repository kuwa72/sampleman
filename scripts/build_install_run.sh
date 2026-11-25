#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$SCRIPT_DIR"

# 1. Ensure PATH contains cargo and mingw
export PATH="$HOME/.cargo/bin:/home/linuxbrew/.linuxbrew/bin:$PATH"

echo "[1/3] Building Sampleman for Windows (x86_64-pc-windows-gnu)..."
cargo build --release --target x86_64-pc-windows-gnu

# 2. Define install destination (Windows %LOCALAPPDATA%\Programs)
# Resolve the real Windows profile via cmd.exe instead of guessing the
# user name; fall back to WIN_USER env (default ykuwa) when unavailable.
INSTALL_BASE_WSL=""
if LOCALAPPDATA_WIN="$(cmd.exe /c 'echo %LOCALAPPDATA%' </dev/null 2>/dev/null | tr -d '\r\n')" \
    && [[ "$LOCALAPPDATA_WIN" == ?:\\* ]]; then
    INSTALL_BASE_WSL="$(wslpath -u "$LOCALAPPDATA_WIN" 2>/dev/null || true)"
    INSTALL_BASE_WIN="$LOCALAPPDATA_WIN"
fi
if [ -z "$INSTALL_BASE_WSL" ]; then
    WIN_USER="${WIN_USER:-ykuwa}"
    INSTALL_BASE_WSL="/mnt/c/Users/${WIN_USER}/AppData/Local"
    INSTALL_BASE_WIN="C:\\Users\\${WIN_USER}\\AppData\\Local"
fi
INSTALL_DIR_WSL="${INSTALL_BASE_WSL}/Programs/Sampleman"
INSTALL_DIR_WIN="${INSTALL_BASE_WIN}\\Programs\\Sampleman"

echo "[2/3] Installing to ${INSTALL_DIR_WIN}..."
mkdir -p "${INSTALL_DIR_WSL}"
cp target/x86_64-pc-windows-gnu/release/sampleman.exe "${INSTALL_DIR_WSL}/sampleman.exe"
if [ -f "soundfont.sf2" ]; then
    cp soundfont.sf2 "${INSTALL_DIR_WSL}/soundfont.sf2"
fi

# 3. Launch via PowerShell Start-Process: `cmd /c "start /d ..."` from WSL
# mis-parses the quoted /d argument as the window title, so the exe is
# looked up in C:\Windows and the launch fails with "file not found".
echo "[3/3] Launching Sampleman on Windows..."
nohup powershell.exe -NoProfile -Command \
    "Start-Process -FilePath '${INSTALL_DIR_WIN}\\sampleman.exe' -WorkingDirectory '${INSTALL_DIR_WIN}'" \
    </dev/null >/dev/null 2>&1 &

echo "Done! Sampleman installed and launched successfully."
