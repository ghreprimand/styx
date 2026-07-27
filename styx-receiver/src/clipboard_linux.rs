//! Clipboard access for the Linux receiver, via `wl-clipboard`.
//!
//! This module deliberately exposes the union of the two macOS clipboard
//! modules' surfaces (`clipboard` and `clipboard_image`) so `main.rs` can
//! bind both names to it and keep every call site platform-agnostic.
//!
//! The blocking functions mirror `clipboard_image.rs` signature-for-signature
//! because `main.rs` invokes them inside `spawn_blocking`. On macOS those are
//! genuinely blocking AppKit calls; here they wrap a short-lived subprocess.
//!
//! Hash kind bytes match the sender and the macOS receiver exactly (0 text,
//! 1 image, 2 html), so dedup state stays consistent across a mixed chain.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::{Command as SyncCommand, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

const TEXT_TIMEOUT: Duration = Duration::from_secs(1);

/// Cap clipboard text at 1 MiB, matching the sender.
const MAX_TEXT_LEN: usize = 1024 * 1024;

/// Cap images just under styx-proto's 32 MiB frame limit.
pub const MAX_IMAGE_LEN: usize = 32 * 1024 * 1024 - 1024;

/// The single image MIME type styx moves over the wire.
pub const IMAGE_MIME: &str = "image/png";

static WL_PASTE: &str = "wl-paste";
static WL_COPY: &str = "wl-copy";

pub fn check_tools() {
    if which(WL_PASTE).is_none() || which(WL_COPY).is_none() {
        log::warn!("wl-paste/wl-copy not found; clipboard sync disabled");
    }
}

fn which(name: &str) -> Option<()> {
    SyncCommand::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

/// Hash for rich-text (HTML) clipboard content. Kind byte 2 keeps these
/// hashes separate from text (0) and image (1) so dedup state is stable
/// across type transitions.
pub fn hash_html(html: &str, plain: &str) -> u64 {
    let mut h = DefaultHasher::new();
    2u8.hash(&mut h);
    html.hash(&mut h);
    plain.hash(&mut h);
    h.finish()
}

// --- async surface, mirroring the macOS `clipboard` module ---

pub async fn read_clipboard() -> Option<String> {
    let result = time::timeout(
        TEXT_TIMEOUT,
        Command::new(WL_PASTE)
            .args(["--no-newline", "--type", "text/plain"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let Ok(mut child) = child else { return };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
        drop(stdin);
    }

    let _ = time::timeout(TEXT_TIMEOUT, child.wait()).await;
}

// --- blocking surface, mirroring the macOS `clipboard_image` module ---

/// List the MIME types the Wayland clipboard currently offers.
fn list_types_blocking() -> Vec<String> {
    let out = SyncCommand::new(WL_PASTE)
        .arg("--list-types")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn read_typed_blocking(mime: &str) -> Option<Vec<u8>> {
    let out = SyncCommand::new(WL_PASTE)
        .args(["--no-newline", "--type", mime])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

pub fn read_clipboard_image() -> Option<(String, Vec<u8>)> {
    if !list_types_blocking().iter().any(|t| t == IMAGE_MIME) {
        return None;
    }
    let out = SyncCommand::new(WL_PASTE)
        .args(["--type", IMAGE_MIME])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    if out.stdout.len() > MAX_IMAGE_LEN {
        log::warn!(
            "clipboard image too large to sync: {} bytes (cap {} bytes)",
            out.stdout.len(),
            MAX_IMAGE_LEN,
        );
        return None;
    }
    Some((IMAGE_MIME.to_string(), out.stdout))
}

pub fn read_clipboard_html() -> Option<(String, String)> {
    let types = list_types_blocking();
    if !types.iter().any(|t| t == "text/html") {
        return None;
    }
    let html_bytes = read_typed_blocking("text/html")?;
    if html_bytes.is_empty() || html_bytes.len() > MAX_TEXT_LEN {
        return None;
    }
    let html = String::from_utf8_lossy(&html_bytes).into_owned();
    let plain = read_typed_blocking("text/plain")
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    Some((html, plain))
}

fn write_blocking(args: &[&str], data: &[u8]) {
    use std::io::Write;
    let child = SyncCommand::new(WL_COPY)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        log::warn!("failed to spawn wl-copy");
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(data);
        drop(stdin);
    }
    let _ = child.wait();
}

pub fn write_clipboard_image(format: &str, data: &[u8]) {
    if format != IMAGE_MIME {
        log::warn!("unsupported clipboard image format '{format}'; dropping");
        return;
    }
    if data.is_empty() {
        return;
    }
    write_blocking(&["--type", IMAGE_MIME], data);
}

/// Write rich text. `wl-copy` accepts only one MIME type per invocation, so
/// the plain-text fallback is what actually lands on the clipboard -- the
/// same asymmetry the macOS-to-Linux direction already has. Writing the HTML
/// instead would leave plain-text consumers staring at raw markup.
pub fn write_clipboard_html(html: &str, plain: &str) {
    let body = if plain.is_empty() { html } else { plain };
    if body.is_empty() {
        return;
    }
    write_blocking(&[], body.as_bytes());
}

/// Change indicator for the proactive clipboard poll.
///
/// macOS has `NSPasteboard.changeCount`, a free monotonic counter. Wayland
/// offers no equivalent, so this hashes the clipboard's offered types plus its
/// text content and returns that as a pseudo-counter -- `main.rs` only ever
/// tests it for inequality, so any value that changes with the content works.
///
/// Reading the clipboard means spawning `wl-paste`, and `main.rs` polls at
/// 10 Hz. To avoid twenty subprocesses a second the real read is throttled to
/// `POLL_THROTTLE`, returning the cached value in between. The cost is up to
/// half a second of extra latency before a copy made on this machine is
/// forwarded, which is invisible next to the time it takes to move the cursor.
const POLL_THROTTLE: Duration = Duration::from_millis(500);

static POLL_STATE: Mutex<Option<(Instant, isize)>> = Mutex::new(None);

pub fn pasteboard_change_count() -> isize {
    let mut guard = match POLL_STATE.lock() {
        Ok(g) => g,
        // A poisoned lock only means a previous poll panicked; the cached
        // value is still meaningful, so recover rather than propagate.
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some((at, value)) = *guard {
        if at.elapsed() < POLL_THROTTLE {
            return value;
        }
    }

    let mut h = DefaultHasher::new();
    for t in list_types_blocking() {
        t.hash(&mut h);
    }
    if let Some(bytes) = read_typed_blocking("text/plain") {
        bytes.hash(&mut h);
    }
    let value = h.finish() as isize;

    *guard = Some((Instant::now(), value));
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    // Kind bytes must keep the three hash families disjoint, otherwise dedup
    // state collides when the clipboard switches content type.
    #[test]
    fn hash_kinds_do_not_collide() {
        let t = hash_text("x");
        let i = hash_image("image/png", b"x");
        let h = hash_html("x", "x");
        assert_ne!(t, i);
        assert_ne!(t, h);
        assert_ne!(i, h);
    }

    #[test]
    fn hash_text_is_stable_and_sensitive() {
        assert_eq!(hash_text("hello"), hash_text("hello"));
        assert_ne!(hash_text("hello"), hash_text("hellp"));
    }

    #[test]
    fn hash_html_distinguishes_plain_fallback() {
        assert_ne!(hash_html("<b>a</b>", "a"), hash_html("<b>a</b>", "b"));
    }

    // The image cap must stay under styx-proto's frame ceiling or an
    // oversized payload would be rejected at encode time instead of being
    // skipped cleanly here.
    #[test]
    fn image_cap_below_frame_limit() {
        assert!(MAX_IMAGE_LEN < 32 * 1024 * 1024);
    }

    // hash_text and the macOS receiver's hash_text must agree, since a mixed
    // chain dedups against values produced on the other platform. Both use
    // DefaultHasher with the same kind byte, so equal input must hash equal
    // within a single build -- this guards accidental reordering of the
    // kind-byte write, which would silently break cross-node dedup.
    #[test]
    fn kind_byte_precedes_payload() {
        let mut a = DefaultHasher::new();
        0u8.hash(&mut a);
        "abc".hash(&mut a);
        assert_eq!(a.finish(), hash_text("abc"));
    }
}
