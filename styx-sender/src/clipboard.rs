use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

const TEXT_TIMEOUT: Duration = Duration::from_secs(1);
const IMAGE_TIMEOUT: Duration = Duration::from_secs(3);

/// Cap clipboard text at 1 MiB. Well over any sane text clipboard use
/// and far below the 32 MiB frame cap.
const MAX_TEXT_LEN: usize = 1 * 1024 * 1024;

/// Cap clipboard images at 32 MiB minus a small header reserve so the
/// encoded frame fits under styx-proto's MAX_FRAME_PAYLOAD.
pub const MAX_IMAGE_LEN: usize = 32 * 1024 * 1024 - 1024;

/// MIME type styx transfers for image clipboard. PNG is lossless,
/// universal, and both wl-clipboard and macOS NSPasteboard handle it
/// natively without conversion.
pub const IMAGE_MIME: &str = "image/png";

static WL_PASTE: &str = "wl-paste";
static WL_COPY: &str = "wl-copy";

pub fn check_tools() {
    if which(WL_PASTE).is_none() || which(WL_COPY).is_none() {
        log::warn!("wl-paste/wl-copy not found; clipboard sync disabled");
    }
}

fn which(name: &str) -> Option<()> {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| ())
}

pub fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    0u8.hash(&mut h); // kind byte so image and text hashes never collide
    text.hash(&mut h);
    h.finish()
}

pub fn hash_image(format: &str, data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    1u8.hash(&mut h);
    format.hash(&mut h);
    data.hash(&mut h);
    h.finish()
}

pub async fn read_clipboard() -> Option<String> {
    let result = time::timeout(
        TEXT_TIMEOUT,
        Command::new(WL_PASTE)
            .args(["--no-newline", "--type", "text/plain"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            if text.is_empty() || text.len() > MAX_TEXT_LEN {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

pub async fn write_clipboard(text: &str) {
    let child = Command::new(WL_COPY)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    let Ok(mut child) = child else { return };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
        drop(stdin);
    }

    let _ = time::timeout(TEXT_TIMEOUT, child.wait()).await;
}

/// List MIME types currently offered by the Wayland clipboard. Used to
/// decide whether an image payload is worth reading at all.
async fn list_clipboard_types() -> Vec<String> {
    let result = time::timeout(
        TEXT_TIMEOUT,
        Command::new(WL_PASTE)
            .arg("--list-types")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Read a PNG from the Wayland clipboard if one is available. Returns
/// `None` for text-only clipboards, empty clipboards, or images over
/// `MAX_IMAGE_LEN`.
pub async fn read_clipboard_image() -> Option<(String, Vec<u8>)> {
    let types = list_clipboard_types().await;
    if !types.iter().any(|t| t == IMAGE_MIME) {
        return None;
    }

    let result = time::timeout(
        IMAGE_TIMEOUT,
        Command::new(WL_PASTE)
            .args(["--type", IMAGE_MIME])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            let data = output.stdout;
            if data.is_empty() {
                None
            } else if data.len() > MAX_IMAGE_LEN {
                log::warn!(
                    "clipboard image too large to sync: {} bytes (cap {} bytes)",
                    data.len(),
                    MAX_IMAGE_LEN,
                );
                None
            } else {
                Some((IMAGE_MIME.to_string(), data))
            }
        }
        _ => None,
    }
}

pub async fn write_clipboard_image(format: &str, data: &[u8]) {
    if format != IMAGE_MIME {
        log::warn!("unsupported clipboard image format '{format}'; dropping");
        return;
    }
    if data.is_empty() {
        return;
    }

    let child = Command::new(WL_COPY)
        .args(["--type", IMAGE_MIME])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    let Ok(mut child) = child else {
        log::warn!("failed to spawn wl-copy for image clipboard");
        return;
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(data).await;
        drop(stdin);
    }

    let _ = time::timeout(IMAGE_TIMEOUT, child.wait()).await;
}
