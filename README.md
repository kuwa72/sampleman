# sampleman

A cross-platform desktop sample/audio library manager for DTM (Desktop Music) production.

> **TL;DR** — sampleman is a fast, offline-native sample library browser for musicians who work with large collections of WAV/MP3/FLAC/MIDI loops and one-shots. It scans folders, extracts BPM/key/instruments from filenames, detects duplicates via content hashing, and provides a searchable table with waveform previews, MIDI piano-roll support, and playback controls.

> 日本語ドキュメント: [README.ja.md](README.ja.md)

---

## Features

### Core
- **Fast folder scanning** — recursive scan of WAV, MP3, FLAC, AIFF, M4A, OGG, WMA, AAC, MIDI
- **Waveform preview** — whole-file waveform rendered as 1200-peak summary
- **Playback engine** — Symphonia-based audio + `rodio` sink with MIDI synthesis via `rustysynth`
- **SQLite database** — persistent track metadata, favorites, and sort/filter state
- **Fuzzy search** — fast fuzzy matching with 180 ms input debounce

### Music metadata (DTM)
- **Filename parsing** — extract BPM, key, and instrument from common pack naming conventions (e.g. `120BPM Am Piano Loop.wav`)
- **Favorites** — star-toggle tracks with persistent storage
- **Duplicate detection** — FNV-1a content-hash identifies identical files across the library
- **Sort & filter** — by BPM, Key, Artist, Album, or favorites

### MIDI
- **Piano-roll rendering** — MIDI tracks show a piano-roll waveform in the track list
- **Drum / Synth channel mode** — select GM sound set per MIDI playback
- **MIDI seek** — seek within MIDI playback via the progress bar

### UI & UX
- **Scan progress** — staged progress display (Scanning → Analyzing → Saving), cancel button, completion summary
- **Error visibility** — walk errors and failed batches counted and surfaced in status
- **Tree state persistence** — expanded folders and selected folder restored on relaunch
- **Panic-safe** — eliminates `panic`/`expect`/`unwrap` from runtime paths

---

## Screenshots

> Screenshots will be added after the first release build.

---

## Building from source

### Prerequisites

| Platform | Requirements |
|----------|-------------|
| **Linux** | `pkg-config`, `libasound2-dev`, `libfontconfig1-dev`, `libglib2.0-dev`, `libgtk-3-dev` |
| **macOS** | Xcode Command Line Tools (`xcode-select --install`) |
| **Windows** | Visual Studio 2022 (C++ build tools) |

### Build

```bash
git clone https://github.com/kuwa72/sampleman.git
cd sampleman
cargo build --release
```

The binary will be at `target/release/sampleman`.

### Run tests

```bash
bash test/run-tests.sh
```

Pure-logic modules (music meta parsing, MIDI utilities) carry `#[cfg(test)]` unit tests. GUI event handlers and audio-device paths are excluded from automated testing.

---

## Usage

1. **Add a library folder** — click the folder icon or drag a directory onto the window.
2. **Wait for scan** — sampleman recursively scans for audio files and indexes them.
3. **Browse & search** — use the search bar (fuzzy matching) or sort by any column.
4. **Preview** — click the play button on any track to audition; drag the progress bar to seek.
5. **Organize** — toggle favorites (star) and use BPM/Key columns to filter your collection.

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| <Space> | Play / pause selected track |
| Delete / Fn+Delete | Seek backward / forward 5 seconds |
| Up / Down | Navigate track list / folders |
| Ctrl/Cmd + F | Focus search |

---

## Project structure

```
sampleman/
├── src/               # Core Rust: UI glue, audio, database, scanning, music meta
├── src/midi_util.rs   # MIDI parsing, piano-roll, GM instrument tables
├── src/music_meta.rs  # Filename BPM/key/instrument parser
├── ui/main.slint      # Slint UI definition
├── build.rs           # Slint UI compilation + Windows icon
├── icons/             # App icons (Windows ICO, macOS ICNS, various sizes)
├── test/
│   └── run-tests.sh   # Test runner (CI + local)
├── Cargo.toml         # Package manifest
└── .github/workflows/ # CI + release workflows
```

---

## License

This project is licensed under the MIT License — see the bundled `LICENSE` file or the repository's license dropdown on GitHub.

---

## Development

See [`AGENTS.md`](./AGENTS.md) for the issue-driven development workflow.

### Dependencies

| Category | Key crates |
|----------|-----------|
| UI | `slint` 1.7, `winit` |
| Audio | `symphonia`, `rodio`, `rustysynth`, `midly` |
| Database | `rusqlite` (bundled SQLite) |
| Scanning | `walkdir`, `rayon` (parallel) |
| Search | `fuzzy-matcher` |
| IPC | `crossbeam-channel`, `tokio` |

---

## Supported Platforms

- **Linux** (x86_64) — tested on Ubuntu 22.04+
- **macOS** (Apple Silicon + Intel)
- **Windows** (x86_64)

---

## Contributing

Contributions are welcome! Please open an issue first to discuss proposed changes.

1. Fork the repository
2. Create a feature branch (`git checkout -b issue/<number>-<slug>`)
3. Write tests for new logic (`test/run-tests.sh`)
4. Ensure all CI checks pass
5. Open a PR
