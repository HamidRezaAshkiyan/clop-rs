use crate::config::AppConfig;
use crate::optimizer::{generate_thumbnail, OptimizedImage};
use anyhow::{bail, Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Returns `true` if `path` has a supported video file extension.
pub fn is_video_path(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4" | "mov" | "mkv" | "webm" | "m4v" | "avi"
    )
}

/// Locates the `ffmpeg` binary on macOS, Windows, or Linux.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(custom) = env::var("CLOP_FFMPEG_PATH") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Some(p);
        }
    }

    // Check common installation paths on macOS and Windows first (GUI apps on macOS have minimal PATH)
    #[cfg(target_os = "macos")]
    {
        for candidate in [
            "/opt/homebrew/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/opt/local/bin/ffmpeg",
        ] {
            let p = PathBuf::from(candidate);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app) = dirs::data_local_dir() {
            let candidates = [
                local_app
                    .join("Programs")
                    .join("clop-rs")
                    .join("ffmpeg.exe"),
                local_app
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links")
                    .join("ffmpeg.exe"),
            ];
            for p in candidates {
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        for candidate in [
            r"C:\ProgramData\chocolatey\bin\ffmpeg.exe",
            r"C:\ffmpeg\bin\ffmpeg.exe",
        ] {
            let p = PathBuf::from(candidate);
            if p.is_file() {
                return Some(p);
            }
        }
    }

    // Check system PATH
    let bin_name = if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let full = dir.join(bin_name);
            if full.is_file() {
                return Some(full);
            }
        }
    }

    None
}

/// Extracts a 64x64 RGBA preview thumbnail from a video file (using `ffmpeg` if available,
/// or falling back to a crisp procedural video-reel icon).
pub fn extract_video_thumbnail_64(input_path: &Path) -> Vec<u8> {
    if let Some(ffmpeg) = find_ffmpeg() {
        let output = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                "00:00:00.200",
                "-i",
            ])
            .arg(input_path)
            .args(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"])
            .output();

        if let Ok(out) = output {
            if out.status.success() && !out.stdout.is_empty() {
                if let Ok(img) = image::load_from_memory(&out.stdout) {
                    let rgba = img.into_rgba8();
                    return generate_thumbnail(rgba.width(), rgba.height(), rgba.as_raw(), 64);
                }
            }
        }
    }
    procedural_video_icon_64()
}

/// Optimizes a video file (`.mp4`, `.mov`, `.mkv`, `.webm`, `.m4v`, `.avi`) using `ffmpeg`.
///
/// Uses hardware-accelerated H.264 (`h264_videotoolbox` on macOS) with fallback to `libx264`,
/// strips metadata if `config.strip_metadata` is enabled, and guarantees non-increasing file size.
pub fn optimize_video_file(
    input_path: &Path,
    output_path: Option<&Path>,
    config: &AppConfig,
) -> Result<OptimizedImage> {
    let start = Instant::now();
    let ffmpeg = find_ffmpeg().context(
        "FFmpeg not found. Install ffmpeg (e.g. 'brew install ffmpeg' on macOS or 'winget install Gyan.FFmpeg' on Windows) to optimize videos.",
    )?;

    let orig_meta = fs::metadata(input_path)
        .with_context(|| format!("Failed to stat video file: {}", input_path.display()))?;
    let original_size = orig_meta.len() as usize;
    if original_size == 0 {
        bail!("Video file is empty: {}", input_path.display());
    }

    let target_path = output_path.unwrap_or(input_path);
    let ext = target_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4")
        .to_ascii_lowercase();

    // Create a temporary output file in the same directory (or system temp dir) with `.mp4` or matching container
    let container_ext = match ext.as_str() {
        "webm" => "webm",
        "mkv" => "mkv",
        _ => "mp4",
    };
    let tmp_path = env::temp_dir().join(format!(
        "clop_rs_video_{}.{container_ext}",
        std::process::id()
    ));
    let _ = fs::remove_file(&tmp_path);

    let crf = config.effective_video_crf();
    let vt_q = config.effective_videotoolbox_q();

    // Build optional scale filter
    let scale_filter = build_video_scale_filter(config);

    // Try libx264 first (universally consistent CRF compression) or VideoToolbox on macOS if libx264 is unavailable
    let encoded_ok = run_ffmpeg_encode(
        &ffmpeg,
        input_path,
        &tmp_path,
        container_ext,
        crf,
        vt_q,
        scale_filter.as_deref(),
        config.strip_metadata,
        false,
    ) || (cfg!(target_os = "macos")
        && run_ffmpeg_encode(
            &ffmpeg,
            input_path,
            &tmp_path,
            container_ext,
            crf,
            vt_q,
            scale_filter.as_deref(),
            config.strip_metadata,
            true,
        ));

    if !encoded_ok || !tmp_path.exists() {
        let _ = fs::remove_file(&tmp_path);
        bail!(
            "FFmpeg failed to encode video file: {}",
            input_path.display()
        );
    }

    let candidate_bytes = fs::read(&tmp_path).context("Failed to read optimized temp video")?;
    let _ = fs::remove_file(&tmp_path);

    let thumb_64 = extract_video_thumbnail_64(input_path);

    if candidate_bytes.len() < original_size || output_path.is_some() {
        fs::write(target_path, &candidate_bytes).with_context(|| {
            format!(
                "Failed to write optimized video to {}",
                target_path.display()
            )
        })?;
        let optimized_size = candidate_bytes.len();
        Ok(OptimizedImage {
            png_bytes: Vec::new(),
            rgba_bytes: thumb_64,
            width: 0,
            height: 0,
            original_size,
            optimized_size,
            was_modified: optimized_size < original_size,
            exif_stripped: config.strip_metadata,
            elapsed: start.elapsed(),
        })
    } else {
        Ok(OptimizedImage {
            png_bytes: Vec::new(),
            rgba_bytes: thumb_64,
            width: 0,
            height: 0,
            original_size,
            optimized_size: original_size,
            was_modified: false,
            exif_stripped: false,
            elapsed: start.elapsed(),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ffmpeg_encode(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    container_ext: &str,
    crf: &str,
    vt_q: &str,
    scale_filter: Option<&str>,
    strip_metadata: bool,
    use_videotoolbox: bool,
) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input);

    if let Some(vf) = scale_filter {
        cmd.args(["-vf", vf]);
    }

    if container_ext == "webm" {
        cmd.args(["-c:v", "libvpx-vp9", "-crf", crf, "-b:v", "0"]);
    } else if use_videotoolbox {
        cmd.args(["-c:v", "h264_videotoolbox", "-q:v", vt_q, "-tag:v", "avc1"]);
    } else {
        cmd.args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", crf, "-pix_fmt", "yuv420p",
        ]);
    }

    if container_ext == "mp4" {
        cmd.args(["-movflags", "+faststart"]);
    }

    cmd.args(["-c:a", "aac", "-b:a", "128k"]);

    if strip_metadata {
        cmd.args(["-map_metadata", "-1"]);
    }

    cmd.arg(output);

    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn build_video_scale_filter(config: &AppConfig) -> Option<String> {
    if let Some(scale) = config.scale_factor {
        if scale > 1.0 {
            return Some(format!("scale=trunc(iw/{scale}/2)*2:trunc(ih/{scale}/2)*2"));
        }
    }
    if let Some(max_w) = config.max_width {
        if max_w > 0 {
            return Some(format!("scale='min({max_w},iw)':-2"));
        }
    }
    None
}

/// Generates a 64x64 RGBA icon showing a sleek play triangle / film card when FFmpeg thumbnailing is skipped.
pub fn procedural_video_icon_64() -> Vec<u8> {
    let mut rgba = vec![0u8; 64 * 64 * 4];
    for y in 0..64u32 {
        for x in 0..64u32 {
            let idx = ((y * 64 + x) * 4) as usize;
            // Play triangle centered at (32, 32)
            let in_play = (22..=44).contains(&x)
                && (18..=46).contains(&y)
                && (y as i32 - 32).unsigned_abs() * 2 <= (44 - x);
            let color = if in_play {
                [52, 211, 153, 255]
            } else if !(8..56).contains(&x) {
                // Film strip perforations
                if (y % 8) < 4 {
                    [51, 65, 85, 255]
                } else {
                    [15, 23, 42, 255]
                }
            } else {
                [24, 34, 52, 255]
            };
            rgba[idx..idx + 4].copy_from_slice(&color);
        }
    }
    rgba
}
