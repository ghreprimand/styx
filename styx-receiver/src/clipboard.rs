use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

const TIMEOUT: Duration = Duration::from_secs(1);
const MAX_TEXT_LEN: usize = 65531; // 65535 frame - 4 byte text length header

pub fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

pub async fn read_clipboard() -> Option<String> {
    let result = time::timeout(
        TIMEOUT,
        Command::new("pbpaste")
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
    let child = Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    let Ok(mut child) = child else { return };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes()).await;
        drop(stdin);
    }

    let _ = time::timeout(TIMEOUT, child.wait()).await;
}
