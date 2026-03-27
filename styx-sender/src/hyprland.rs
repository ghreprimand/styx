use std::env;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Deserialize)]
struct Monitor {
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    transform: i32,
}

pub struct MonitorGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

fn socket_path() -> Result<String, Box<dyn std::error::Error>> {
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE")?;
    let xdg = env::var("XDG_RUNTIME_DIR")?;
    Ok(format!("{}/hypr/{}/.socket.sock", xdg, sig))
}

async fn hyprctl(command: &str) -> Result<String, Box<dyn std::error::Error>> {
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).await?;
    stream.write_all(command.as_bytes()).await?;
    stream.shutdown().await?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await?;
    Ok(buf)
}

pub async fn get_monitor(name: &str) -> Result<MonitorGeometry, Box<dyn std::error::Error>> {
    let json = hyprctl("j/monitors").await?;
    let monitors: Vec<Monitor> = serde_json::from_str(&json)?;
    let mon = monitors
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| format!("monitor '{}' not found via Hyprland IPC", name))?;
    // Hyprland reports native (pre-rotation) width/height.
    // Swap for 90° (1) and 270° (3) transforms.
    let (w, h) = if mon.transform == 1 || mon.transform == 3 {
        (mon.height, mon.width)
    } else {
        (mon.width, mon.height)
    };
    Ok(MonitorGeometry {
        x: mon.x,
        y: mon.y,
        width: w,
        height: h,
    })
}

pub async fn warp_cursor(x: i32, y: i32) -> Result<(), Box<dyn std::error::Error>> {
    let cmd = format!("/dispatch movecursor {} {}", x, y);
    hyprctl(&cmd).await?;
    Ok(())
}
