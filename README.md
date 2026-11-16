# clop-rs ⚡

A minimal-footprint, cross-platform (**macOS** & **Windows**) automatic clipboard image optimizer written in pure **Rust**, inspired by [FuzzyIdeas/Clop](https://github.com/FuzzyIdeas/Clop).

Whenever you copy an image or screenshot to your clipboard, `clop-rs` silently optimizes it in-place in milliseconds (typically **60–85% smaller**) so you can immediately paste tiny, fast-loading images into Slack, Discord, GitHub, Notion, browsers, or emails.

---

## Why `clop-rs`?

- **Cross-Platform (macOS & Windows)**: Works natively on both macOS (`NSPasteboard`) and Windows (`Win32 Clipboard`).
- **Ultra-Minimal Footprint**:
  - **Zero-allocation idle check**: Uses OS clipboard sequence counters (`NSPasteboard.changeCount` on macOS, `GetClipboardSequenceNumber()` on Windows) so polling while idle takes **0% CPU** and **0 allocations**.
  - **Tiny memory usage**: ~4–8 MB RSS in the background.
  - **Single standalone binary**: ~2.5 MB stripped native executable with zero external CLI tool dependencies (`pngquant`, `oxipng`, etc. are compiled directly into the binary).
- **Direct Native PNG Clipboard Preservation**:
  - Writes the exact compressed PNG stream directly to `public.png` on macOS and the registered `PNG` clipboard format (alongside `CF_DIBV5`) on Windows, so pasting apps receive the exact quantized + deflate-optimized PNG bytes without re-encoding.
- **Two Optimization Modes**:
  - **Smart Mode (Default)**: Uses `imagequant` (the engine behind `pngquant` and `Clop`) for perceptual 8-bit RGBA palette quantization followed by `oxipng` deflate compression (**60–85% size reduction** with virtually zero perceptual quality loss).
  - **Strict Lossless Mode (`--lossless`)**: Uses `oxipng` + `libdeflater` to strip metadata and optimize PNG deflate streams while preserving 100% bit-exact 32-bit RGBA pixel values.
- **Flexible Runtime Modes**:
  - **System Tray / Menu Bar App** (default): Lightweight tray menu to pause/resume, toggle Smart vs. Lossless mode, view live savings statistics, optimize on demand, or toggle Launch at Login.
  - **Headless Daemon (`--headless`)**: Pure background process with zero UI.
  - **One-Shot CLI (`--once` / `-1`)**: Optimize the current clipboard image once and exit (great for hotkeys, Raycast, Alfred, AutoHotkey, or PowerToys).
  - **File Optimizer (`clop-rs file <paths>`)**: Optimize PNG, JPEG, WebP, BMP, or TIFF files on disk.

---

## Installation & Build

### Option 1: Download Prebuilt Installer (Recommended)
Download the latest installer from [**GitHub Releases**](https://github.com/HamidRezaAshkiyan/clop-rs/releases/latest):

- **macOS (`.pkg` Installer)**: [`clop-rs-v0.1.0-macos-arm64.pkg`](https://github.com/HamidRezaAshkiyan/clop-rs/releases/latest/download/clop-rs-v0.1.0-macos-arm64.pkg) — Installs `/Applications/clop-rs.app` and `/usr/local/bin/clop-rs`.
- **macOS (`.dmg` Disk Image)**: [`clop-rs-v0.1.0-macos-arm64.dmg`](https://github.com/HamidRezaAshkiyan/clop-rs/releases/latest/download/clop-rs-v0.1.0-macos-arm64.dmg) — Drag-and-drop `clop-rs.app` to `Applications`.
- **Windows (`.exe` / `.zip`)**: [`clop-rs-v0.1.0-windows-x64.exe`](https://github.com/HamidRezaAshkiyan/clop-rs/releases/latest/download/clop-rs-v0.1.0-windows-x64.exe) / [`clop-rs-v0.1.0-windows-x64.zip`](https://github.com/HamidRezaAshkiyan/clop-rs/releases/latest/download/clop-rs-v0.1.0-windows-x64.zip)

### Option 2: One-Line Terminal Installer

**macOS**:
```bash
curl -fsSL https://github.com/HamidRezaAshkiyan/clop-rs/releases/latest/download/install.sh | bash
```

**Windows (PowerShell)**:
```powershell
irm https://github.com/HamidRezaAshkiyan/clop-rs/releases/latest/download/install.ps1 | iex
```

### Option 3: Build from Source

```bash
git clone https://github.com/HamidRezaAshkiyan/clop-rs.git
cd clop-rs
cargo build --release
```

The optimized binary will be generated at:
- **macOS**: `./target/release/clop-rs`
- **Windows**: `.\target\release\clop-rs.exe` (compiled with `windows_subsystem = "windows"` so no console window flashes when launched in the background)

---

## Usage

### 1. Run in Menu Bar / System Tray Mode (Default)
```bash
clop-rs
```

### 2. Run as a Pure Headless Daemon
```bash
clop-rs --headless
```

### 3. Optimize the Current Clipboard Once and Exit
```bash
clop-rs --once
```

### 4. Strict Lossless Mode or Custom Quality / Downscaling
```bash
# Strict lossless mode (bit-exact pixels)
clop-rs --lossless

# Downscale Retina 2x screenshots by half and optimize
clop-rs --scale-factor 2.0

# Cap maximum image dimensions to 1920x1080
clop-rs --max-width 1920 --max-height 1080

# Custom perceptual quality target (10-100, default: 95)
clop-rs --quality 85
```

### 5. Optimize Image Files on Disk
```bash
# Optimize one or more files in-place
clop-rs file screenshot1.png screenshot2.png

# Optimize to a new output path
clop-rs file input.png --output output.png
```

### 6. Enable / Disable Launch at Login
```bash
# Enable auto-start (macOS LaunchAgent / Windows Startup)
clop-rs autostart --enable

# Disable auto-start
clop-rs autostart --disable
```

### 7. View or Save Persistent Configuration
Configuration is stored in:
- **macOS**: `~/Library/Application Support/clop-rs/config.toml`
- **Windows**: `%APPDATA%\clop-rs\config.toml`

```bash
# Print current effective configuration
clop-rs config

# Save CLI flags as persistent defaults
clop-rs --quality 90 --max-width 2560 config --save

# Reset configuration to defaults
clop-rs config --reset
```

---

## Architecture

```
┌───────────────────────────────────────────────────────────┐
│        OS Clipboard (macOS NSPasteboard / Win32)          │
└─────────────┬─────────────────────────────────┬───────────┘
              │ O(1) Sequence Counter Check     │
              ▼                                 │
┌───────────────────────────┐                   │
│  ClipboardMonitor         │                   │
│  • Change count guard     │                   │
│  • 64-bit hash ring-buf   │                   │
│    (prevents self-loops)  │                   │
└─────────────┬─────────────┘                   │
              │ New Unoptimized Image           │
              ▼                                 │
┌───────────────────────────┐                   │
│  Optimization Pipeline    │                   │
│  1. Lanczos3 Downscale    │                   │
│  2. imagequant Palette    │                   │
│  3. oxipng Libdeflater    │                   │
│  4. Strict Size Guard     │                   │
└─────────────┴─────────────────────────────────┘
              Writes raw `public.png` / `PNG`
```

---

## License

MIT License
