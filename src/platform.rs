use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;

/// Guard holding the OS single-instance lock for the tray daemon.
/// When dropped, cleans up the lock/socket resource.
pub struct SingleInstanceGuard {
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(windows)]
    _lock_file: fs::File,
}

#[cfg(unix)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Attempts to acquire the single-instance lock for the GUI tray daemon.
///
/// - If no other instance is running, returns `Some(SingleInstanceGuard)` and (on Unix)
///   listens in the background for activation pings from future launches, invoking `on_activate`.
/// - If another instance is already running, signals it to focus its Dashboard (on Unix)
///   and returns `None`.
pub fn acquire_single_instance<F>(on_activate: F) -> Option<SingleInstanceGuard>
where
    F: Fn() + Send + 'static,
{
    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::thread;

        let socket_path = env::temp_dir().join("clop-rs-instance.sock");
        if let Ok(mut stream) = UnixStream::connect(&socket_path) {
            let _ = stream.write_all(b"show\n");
            return None;
        }
        // Stale socket from a previous crashed run
        let _ = fs::remove_file(&socket_path);
        match UnixListener::bind(&socket_path) {
            Ok(listener) => {
                thread::spawn(move || {
                    for mut stream in listener.incoming().flatten() {
                        let mut buf = [0u8; 16];
                        let _ = stream.read(&mut buf);
                        on_activate();
                    }
                });
                Some(SingleInstanceGuard { socket_path })
            }
            Err(err) => {
                log::warn!("Could not bind single-instance socket: {}", err);
                // Proceed rather than failing to launch if temp dir is restricted
                let _ = on_activate;
                None
            }
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        let _ = on_activate;
        let lock_path = env::temp_dir().join("clop-rs-instance.lock");
        match fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(0)
            .open(&lock_path)
        {
            Ok(file) => Some(SingleInstanceGuard { _lock_file: file }),
            Err(_) => None,
        }
    }
}

/// Returns a lightweight OS clipboard sequence / change counter if supported by the OS.
///
/// - On **macOS**: Queries `NSPasteboard.generalPasteboard.changeCount` (O(1), zero allocation).
/// - On **Windows**: Queries `GetClipboardSequenceNumber()` (O(1), zero allocation).
/// - On other platforms: Returns `None` (falls back to image buffer hashing).
pub fn clipboard_change_count() -> Option<i64> {
    #[cfg(target_os = "macos")]
    {
        macos::change_count()
    }
    #[cfg(target_os = "windows")]
    {
        windows::change_count()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Attempts to read raw PNG bytes (`public.png` on macOS, `PNG` format on Windows) directly
/// from the system clipboard without re-encoding.
pub fn read_raw_clipboard_png() -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        macos::read_png()
    }
    #[cfg(target_os = "windows")]
    {
        windows::read_png()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Writes the exact optimized PNG bytes directly to the system clipboard's native PNG format.
///
/// Returns `Ok(true)` if the platform-native write succeeded and handled the clipboard update,
/// or `Ok(false)` if the caller should also use `arboard` fallback.
pub fn write_raw_clipboard_png(png_bytes: &[u8]) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::write_png(png_bytes)
    }
    #[cfg(target_os = "windows")]
    {
        windows::write_png(png_bytes)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = png_bytes;
        Ok(false)
    }
}

/// Hides the Dock icon on macOS so `clop-rs` runs strictly in the Menu Bar / background.
pub fn configure_background_app() {
    #[cfg(target_os = "macos")]
    {
        macos::set_accessory_policy();
    }
}

/// Brings the app's visible windows (such as the Settings/Dashboard window) to the foreground.
pub fn activate_app() {
    #[cfg(target_os = "macos")]
    {
        macos::activate_app();
    }
}

/// Enables or disables automatic startup at user login.
pub fn set_autostart(enable: bool) -> Result<PathBuf> {
    let exe_path = env::current_exe().context("Failed to determine current executable path")?;

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().context("Could not find home directory")?;
        let agents_dir = home.join("Library").join("LaunchAgents");
        let plist_path = agents_dir.join("com.hamidrezaashkiyan.clop-rs.plist");

        if enable {
            fs::create_dir_all(&agents_dir)?;
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.hamidrezaashkiyan.clop-rs</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>
"#,
                exe_path.display()
            );
            fs::write(&plist_path, plist)?;
        } else if plist_path.exists() {
            fs::remove_file(&plist_path)?;
        }
        Ok(plist_path)
    }

    #[cfg(target_os = "windows")]
    {
        let appdata = dirs::data_dir().context("Could not find Roaming AppData directory")?;
        let startup_dir = appdata
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup");
        let script_path = startup_dir.join("clop-rs.vbs");

        if enable {
            fs::create_dir_all(&startup_dir)?;
            let vbs = format!(
                "Set WshShell = CreateObject(\"WScript.Shell\")\r\nWshShell.Run \"\"\"{}\"\"\", 0, False\r\n",
                exe_path.display()
            );
            fs::write(&script_path, vbs)?;
        } else if script_path.exists() {
            fs::remove_file(&script_path)?;
        }
        Ok(script_path)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let config_dir = dirs::config_dir().context("Could not find config directory")?;
        let autostart_dir = config_dir.join("autostart");
        let desktop_path = autostart_dir.join("clop-rs.desktop");
        if enable {
            fs::create_dir_all(&autostart_dir)?;
            let content = format!(
                "[Desktop Entry]\nType=Application\nName=clop-rs\nExec={}\nTerminal=false\n",
                exe_path.display()
            );
            fs::write(&desktop_path, content)?;
        } else if desktop_path.exists() {
            fs::remove_file(&desktop_path)?;
        }
        Ok(desktop_path)
    }
}

/// Checks whether autostart is currently enabled.
pub fn is_autostart_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            return home
                .join("Library")
                .join("LaunchAgents")
                .join("com.hamidrezaashkiyan.clop-rs.plist")
                .exists();
        }
        false
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = dirs::data_dir() {
            return appdata
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("Startup")
                .join("clop-rs.vbs")
                .exists();
        }
        false
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(cfg) = dirs::config_dir() {
            return cfg.join("autostart").join("clop-rs.desktop").exists();
        }
        false
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{bail, Result};
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::{NSData, NSString};

    pub fn change_count() -> Option<i64> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            Some(pb.changeCount() as i64)
        }
    }

    pub fn read_png() -> Option<Vec<u8>> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let png_type = NSString::from_str("public.png");
            let data = pb.dataForType(&png_type)?;
            Some(data.bytes().to_vec())
        }
    }

    pub fn write_png(png_bytes: &[u8]) -> Result<bool> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let png_type = NSString::from_str("public.png");
            let data = NSData::with_bytes(png_bytes);
            let ok = pb.setData_forType(Some(&data), &png_type);
            if !ok {
                bail!("NSPasteboard::setData_forType(public.png) returned false");
            }
            Ok(true)
        }
    }

    pub fn set_accessory_policy() {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_app_kit::NSApplicationActivationPolicy;
        unsafe {
            let cls = objc2::class!(NSApplication);
            let app: *mut AnyObject = msg_send![cls, sharedApplication];
            if !app.is_null() {
                let _: bool = msg_send![
                    app,
                    setActivationPolicy: NSApplicationActivationPolicy::Accessory
                ];
            }
        }
    }

    pub fn activate_app() {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        unsafe {
            let cls = objc2::class!(NSApplication);
            let app: *mut AnyObject = msg_send![cls, sharedApplication];
            if !app.is_null() {
                let _: () = msg_send![app, activateIgnoringOtherApps: true];
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use anyhow::{bail, Result};
    use std::ptr;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };

    fn png_format_id() -> u32 {
        let name: [u16; 4] = [b'P' as u16, b'N' as u16, b'G' as u16, 0];
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    }

    pub fn change_count() -> Option<i64> {
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == 0 {
            None
        } else {
            Some(seq as i64)
        }
    }

    pub fn read_png() -> Option<Vec<u8>> {
        unsafe {
            let cf_png = png_format_id();
            if cf_png == 0 || IsClipboardFormatAvailable(cf_png) == 0 {
                return None;
            }
            if OpenClipboard(0 as HWND) == 0 {
                return None;
            }
            let result = (|| {
                let handle = GetClipboardData(cf_png);
                if handle.is_null() {
                    return None;
                }
                let size = GlobalSize(handle);
                if size == 0 {
                    return None;
                }
                let ptr = GlobalLock(handle) as *const u8;
                if ptr.is_null() {
                    return None;
                }
                let slice = std::slice::from_raw_parts(ptr, size);
                let vec = slice.to_vec();
                GlobalUnlock(handle);
                Some(vec)
            })();
            CloseClipboard();
            result
        }
    }

    /// Appends the registered `"PNG"` format onto the currently-set clipboard (so both `CF_DIBV5`
    /// from `arboard` and compressed `"PNG"` are available to pasting apps).
    pub fn write_png(png_bytes: &[u8]) -> Result<bool> {
        unsafe {
            let cf_png = png_format_id();
            if cf_png == 0 {
                bail!("RegisterClipboardFormatW(PNG) failed");
            }
            if OpenClipboard(0 as HWND) == 0 {
                bail!("OpenClipboard failed");
            }
            // Note: We do NOT call EmptyClipboard() here when invoked after `arboard::set_image`
            // so that `CF_DIB`/`CF_DIBV5` remains available alongside `"PNG"`.
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, png_bytes.len());
            if hglobal.is_null() {
                CloseClipboard();
                bail!("GlobalAlloc failed");
            }
            let dst = GlobalLock(hglobal) as *mut u8;
            if dst.is_null() {
                CloseClipboard();
                bail!("GlobalLock failed");
            }
            ptr::copy_nonoverlapping(png_bytes.as_ptr(), dst, png_bytes.len());
            GlobalUnlock(hglobal);

            let res = SetClipboardData(cf_png, hglobal);
            CloseClipboard();
            if res.is_null() {
                bail!("SetClipboardData(PNG) failed");
            }
            // Return false so caller knows `arboard::set_image` is used in tandem on Windows
            Ok(true)
        }
    }
}
