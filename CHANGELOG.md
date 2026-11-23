# Changelog

All notable changes to `clop-rs` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-11-24

### Added
- **Non-blocking Architecture & Instant Menu/UI Response**:
  - Wired tray `MenuEvent` directly into `EventLoopProxy<AppEvent>` for zero-latency menu clicks and zero-polling `ControlFlow::Wait` when the HUD is hidden.
  - Moved Clipboard polling/optimization, Downloads watching/optimization, Finder/Explorer HUD optimization (`run_file_with_hud`), and OS toggles (`pbs -update` / Windows registry) onto dedicated background worker threads.
  - Added lightweight `light_state` UI IPC updates and cached `SupportedFormatsInfo` so toggling settings or presets never re-serializes history thumbnails or scans `PATH`.
- **Single-Instance Guard**:
  - Prevents launching duplicate tray daemons; a second launch on Unix signals the running instance to focus its Dashboard.
- **Multi-Folder Watching & Ignore Rules**:
  - Added `extra_watch_dirs` and wildcard `ignore_patterns` (`*`, `?`) to `AppConfig` so users can watch multiple folders (e.g., `~/Downloads` + `~/Desktop`) while skipping matching filenames.
- **Schema Versioning**:
  - Added `schema_version = 1` to `AppConfig` and `OptimizationHistory` for forward-compatible `1.x` migrations.
- **Disk-Backed Revert Cache (`DiskBackup`)**:
  - Replaced in-memory `Vec<u8>` file revert buffers with disk-backed cached copies that automatically clean up on drop, preventing RAM spikes on large videos and batches.
- **CI/CD & Packaging Improvements**:
  - Gated GitHub Releases and Homebrew/WinGet manifest updates on explicit `v*` tags **after** macOS and Windows `fmt`, `clippy`, `test`, and release builds succeed.
  - Generated macOS `AppIcon.icns` from `assets/logo.png` and added optional Developer ID code-signing (`APPLE_SIGNING_IDENTITY`) and notarization (`APPLE_NOTARY_KEYCHAIN_PROFILE`) hooks.

### Fixed
- **Native Format Preservation**: `.webp`, `.bmp`, and `.tiff` files are now re-encoded in their native format instead of being overwritten with PNG bytes.
- **Atomic File Writes**: Images, PDFs, videos, `config.toml`, and `history.json` are now written via sibling temp files + `sync_all` + atomic rename, preserving permissions and preventing truncation on crash.
- **PDF Safety Guards**: Encrypted (`/Encrypt`), digitally signed (`/ByteRange`, `/Type /Sig`), and PDF 1.5+ compressed object/xref stream (`/ObjStm`, `/XRef`) documents are safely protected from native rewriting.
- **Collision-Free Temp Files & Zero-RAM Video Move**: Replaced PID-only temp filenames in `pdf.rs` and `video.rs` with unique sequence paths and moved encoded videos atomically instead of buffering them in RAM.
- **Documentation**: Fixed duplicate heading numbers in `README.md` and aligned the macOS config path comment in `src/config.rs`.
