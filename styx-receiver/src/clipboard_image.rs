use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};
use objc2_foundation::{NSData, NSString};

/// MIME type styx transfers for image clipboard. Matches the sender side.
pub const IMAGE_MIME: &str = "image/png";

/// Cap images at 32 MiB minus a small header reserve so the encoded
/// frame fits under styx-proto's MAX_FRAME_PAYLOAD.
pub const MAX_IMAGE_LEN: usize = 32 * 1024 * 1024 - 1024;

/// Hash of the most recent PNG we saw on (or wrote to) the pasteboard.
/// Used solely to detect "the PNG is unchanged since the last sync" so
/// we can defer to the text path. 0 is the sentinel for "none so far";
/// a real PNG hashing to exactly 0 is a 1-in-2^64 collision we accept.
static LAST_PNG_HASH: AtomicU64 = AtomicU64::new(0);

fn hash_bytes(data: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

/// Read a PNG from the macOS general pasteboard, but *only* if the
/// current PNG differs from the last one we saw or wrote. If the PNG
/// is unchanged we return `None`, which lets the text path (pbpaste
/// via `clipboard::read_clipboard()`) handle whatever text the user
/// may have copied on top of it.
///
/// Background: `dataForType(NSPasteboardTypeString)` from a background
/// thread can miss text published via lazy pasteboard providers (e.g.
/// terminal apps) even when the text is actually on the pasteboard.
/// `pbpaste` runs in a subprocess with its own runloop and
/// materialises those providers correctly, and it already handles
/// non-plain text UTIs (RTF, etc.) via its built-in conversion. By
/// deferring to `pbpaste` whenever the PNG is unchanged, we dodge the
/// lazy-provider race without trying to outguess the pasteboard.
///
/// Echo prevention (don't send a PNG we just synced from linux) is
/// already handled by `last_clip_hash` in `main.rs`, so returning
/// `None` here for an unchanged PNG never loses data.
///
/// Size-caps the payload at `MAX_IMAGE_LEN`.
pub fn read_clipboard_image() -> Option<(String, Vec<u8>)> {
    autoreleasepool(|_| {
        let pb = NSPasteboard::generalPasteboard();
        // SAFETY: NSPasteboardTypePNG is a framework-declared extern
        // static; non-null once AppKit is linked in (always, for this
        // binary).
        let png_uti: &NSString = unsafe { NSPasteboardTypePNG };
        let png_data = pb.dataForType(png_uti)?;
        let bytes = png_data.to_vec();
        if bytes.is_empty() {
            return None;
        }
        if bytes.len() > MAX_IMAGE_LEN {
            log::warn!(
                "clipboard image too large to sync: {} bytes (cap {} bytes)",
                bytes.len(),
                MAX_IMAGE_LEN,
            );
            return None;
        }

        let current_hash = hash_bytes(&bytes);
        let last_hash = LAST_PNG_HASH.load(Ordering::Relaxed);
        if current_hash == last_hash {
            // Same PNG we already saw or wrote. Defer to the text path
            // so any newly-copied text on top of this stale PNG wins.
            return None;
        }
        LAST_PNG_HASH.store(current_hash, Ordering::Relaxed);
        Some((IMAGE_MIME.to_string(), bytes))
    })
}

/// Current `NSPasteboard.changeCount`. Monotonic integer bumped by the
/// pasteboard server on any mutation (`clearContents`,
/// `setData:forType:`, `declareTypes:owner:`). Reading it is a cheap
/// IPC round-trip, so callers can poll at ~10 Hz and only do the
/// expensive `dataForType` read when the count changes.
pub fn pasteboard_change_count() -> isize {
    autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount())
}

/// Write a PNG blob to the macOS general pasteboard under `public.png`.
/// `format` must be `image/png`; other formats are dropped with a warn.
/// Updates the snapshot so a read immediately after a write recognises
/// the freshly-written PNG as "already synced, not new user input."
pub fn write_clipboard_image(format: &str, data: &[u8]) {
    if format != IMAGE_MIME {
        log::warn!("unsupported clipboard image format '{format}'; dropping");
        return;
    }
    if data.is_empty() {
        return;
    }

    autoreleasepool(|_| {
        let nsdata = NSData::with_bytes(data);
        // SAFETY: see note in read_clipboard_image.
        let uti: &NSString = unsafe { NSPasteboardTypePNG };
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        pb.setData_forType(Some(&nsdata), uti);
        LAST_PNG_HASH.store(hash_bytes(data), Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a minimal valid PNG through the macOS general
    /// pasteboard. Validates that our NSPasteboard FFI is wired up
    /// correctly before the rest of the receiver ever touches it.
    ///
    /// Note: this test mutates the user's clipboard. Ignored by default
    /// so it only runs with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn ns_pasteboard_round_trip() {
        // Minimal 1x1 transparent PNG.
        let png: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
            0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01,
            0x0D, 0x0A, 0x2D, 0xB4,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND chunk
            0xAE, 0x42, 0x60, 0x82,
        ];
        write_clipboard_image(IMAGE_MIME, &png);
        // write stamps LAST_PNG_HASH for echo prevention; reset so the
        // immediate read below isn't suppressed as a self-echo.
        LAST_PNG_HASH.store(0, Ordering::Relaxed);
        let (mime, round_tripped) = read_clipboard_image().expect("PNG should be on pasteboard");
        assert_eq!(mime, IMAGE_MIME);
        assert_eq!(round_tripped, png);
    }

    /// Writing a PNG and immediately reading it back must return `None`:
    /// rc6's echo prevention treats a freshly-written PNG as already
    /// synced. Guards against accidentally restoring the old behavior.
    #[test]
    #[ignore]
    fn write_then_read_is_suppressed_as_echo() {
        let png: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54,
            0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01,
            0x0D, 0x0A, 0x2D, 0xB4,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
            0xAE, 0x42, 0x60, 0x82,
        ];
        write_clipboard_image(IMAGE_MIME, &png);
        assert!(read_clipboard_image().is_none());
    }
}
