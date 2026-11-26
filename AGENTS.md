# AGENTS.md — clop-rs

## Overview
**clop-rs** (`v1.1.0`) is a minimal-footprint, cross-platform (**macOS & Windows**) automatic clipboard, Downloads folder, image, video, and PDF optimizer written in Rust, inspired by Clop. It watches the OS clipboard using O(1) sequence counters (0% idle CPU), compresses images/videos/PDFs in-place (typically 60–85% smaller), shows a floating HUD panel with thumbnail preview and 1-click `[REVERT]`, and also provides a WebView settings UI, a Ratatui terminal UI (`clop-rs tui`), a dedicated `clop-cli` binary, and Finder/Explorer right-click context menu integration.

## Tech Stack & Requirements
- **Language / Toolchain:** Rust (Edition 2021), library crate `clop_rs` + two binaries (`clop-rs`, `clop-cli`).
- **Image Optimization:** `image` 0.25 (`png`, `jpeg`, `webp`, `bmp`, `tiff`), `imagequant` 4.3 (perceptual palette quantization), `oxipng` 9.1, `mozjpeg` 0.10, `png` 0.17, `flate2`.
- **Video & PDF:** Optional external `ffmpeg` (`h264_videotoolbox` on macOS or `libx264`) for video; pure-Rust PDF stream/JPEG/xref optimizer with optional Ghostscript (`gs`).
- **UI & Tray:** `tao` 0.30 + `wry` 0.47 (floating HUD & embedded HTML UI), `tray-icon` 0.19, `rfd` 0.15, `ratatui` 0.29 + `crossterm` 0.28 (TUI).
- **Platform Native Clipboard:**
  - macOS: `objc2`, `objc2-app-kit` (`NSPasteboard`), `objc2-foundation`, `core-graphics`.
  - Windows: `windows-sys` 0.59 (`Win32_System_DataExchange`, `Win32_Graphics_Gdi`), plus `arboard` 3.4 fallback.
- **CLI & Config:** `clap` 4.5 (`derive`), `serde` / `serde_json` / `toml`, `dirs` 5, `env_logger`, `anyhow`.
- **Release Profile:** `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.

## Repository Structure
```text
clop-rs/
├── Cargo.toml                      # Crate manifest (lib + bins: clop-rs, clop-cli)
├── src/
│   ├── lib.rs                      # Library module exports
│   ├── main.rs                     # `clop-rs` binary entry (tray, HUD, headless, TUI, subcommands)
│   ├── bin/clop-cli.rs             # Dedicated scriptable CLI binary (`clop-cli`)
│   ├── clipboard.rs                # O(1) sequence-number clipboard watcher & direct PNG writer
│   ├── optimizer.rs                # Image optimization pipeline (Smart imagequant+oxipng, Lossless, mozjpeg, EXIF strip)
│   ├── video.rs                    # FFmpeg video compression & frame thumbnail extraction
│   ├── pdf.rs                      # Pure-Rust PDF stream/image optimizer + optional Ghostscript
│   ├── downloads.rs                # ~/Downloads folder watcher for images, videos, PDFs
│   ├── history.rs                  # Optimization history persistence & revert support
│   ├── config.rs                   # AppConfig TOML persistence, CLI flags, presets
│   ├── tray.rs                     # macOS menu bar & Windows system tray event loop
│   ├── hud.rs                      # Floating bottom-right HUD WebView with progress & [REVERT]
│   ├── ui.rs / ui.html             # Embedded desktop settings/dashboard WebView
│   ├── tui.rs                      # Ratatui interactive terminal UI (`clop-rs tui`)
│   ├── context_menu.rs             # macOS Finder Quick Action & Windows Explorer context menu setup
│   ├── platform.rs                 # OS-specific autostart & native helpers
│   └── updater.rs                  # Release update checker
├── tests/integration_tests.rs      # Integration test suite (uses tempfile)
├── scripts/                        # package-macos.sh, install.sh, install.ps1, update-package-manifests.py
├── Casks/ & Formula/               # Homebrew Cask & Formula definitions
├── packaging/winget/               # Winget YAML manifests
└── docs/                           # GitHub Pages site
```

## Architecture & Key Flows
- **Entry Points:**
  - `src/main.rs` (`clop-rs`): Parses `Cli` flags, loads `AppConfig`, and dispatches subcommands (`ui`, `tui`, `history`, `file`, `config`, `autostart`, `context-menu`) or runs in default tray mode (`tray::run_with_tray`), `--headless` daemon, or `--once` one-shot clipboard optimization.
  - `src/bin/clop-cli.rs` (`clop-cli`): Pure CLI interface for batch/scripted image, PDF, video, and clipboard optimization.
- **Clipboard Pipeline (`clipboard.rs` → `optimizer.rs` → `hud.rs`):** Polls `NSPasteboard.changeCount` (macOS) or `GetClipboardSequenceNumber()` (Windows) with zero idle CPU. On change, extracts raw image bytes, runs `optimizer.rs` (Smart `imagequant` + `oxipng`, or `--lossless`), writes compressed PNG bytes directly back to native clipboard (`public.png` / registered `PNG` + `CF_DIBV5`), records original bytes for 1-click revert in `history.rs`, and triggers `hud.rs`.
- **Downloads & Context Menu:** `downloads.rs` watches `~/Downloads` for supported media; Finder/Explorer right-click actions invoke `clop-rs file <paths> --hud`.

## Development & Commands
```bash
# Build both binaries (debug / release)
cargo build
cargo build --release

# Run unit & integration tests
cargo test

# Run main app in different modes
cargo run -- --watch-downloads        # Tray + HUD + Downloads watcher
cargo run -- --headless               # Headless daemon
cargo run -- --once                   # Optimize clipboard once and exit
cargo run -- tui                      # Interactive Ratatui TUI

# Run dedicated CLI binary
cargo run --bin clop-cli -- --help

# Package macOS .app / .pkg / .dmg
./scripts/package-macos.sh
```

## Configuration & Environment
- **Config & History Files:** Persisted in the OS user config/data directory via `dirs` (managed by `config.rs` and `history.rs`).
- **`RUST_LOG`:** Controls `env_logger` verbosity (defaults to `info,oxipng=warn`).
- **Optional External Tools:** `ffmpeg` (for video compression) and `gs` (Ghostscript, optional enhanced PDF compression) are discovered on `PATH`.

## Conventions & Gotchas
- **Windows release subsystem:** `src/main.rs` sets `#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]` so `clop-rs.exe` has no console window in release builds; `clop-cli.exe` remains a console binary.
- **Native `mozjpeg` build:** `mozjpeg` compiles C sources via `cc`/NASM; ensure a working C toolchain is installed.
- **UI thread ownership:** `tao`/`wry`/`tray-icon` must run on the main OS thread; clipboard/downloads watchers and heavy compression run on worker threads.
- **Packaging manifests:** When bumping `Cargo.toml` version, keep `Casks/clop-rs.rb`, `Formula/clop-rs.rb`, and `packaging/winget/` in sync (see `scripts/update-package-manifests.py`).
