//! The binary stopped as a service manager stops it, and as a terminal
//! does: told by a signal while it records, it finishes the recording —
//! its playlist ended — and exits as having succeeded. Unix alone: no
//! test here sends a Windows console its events.

#![cfg(unix)]

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;

/// How long a stop may take: a recording finished, a camera let go of.
const STOPS_WITHIN: Duration = Duration::from_secs(15);

/// Every playlist under `root`, as it is now.
fn playlists(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "m3u8") {
                found.push(std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    found
}

/// A port on the loopback nothing listens on, a moment ago.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The binary, recording a camera into `root`, its API at the address
/// answered with.
fn recording(root: &Path, camera: SocketAddr) -> (Child, SocketAddr) {
    let api: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let config = root.join("relay-pp.toml");
    std::fs::write(
        &config,
        format!(
            "[rtsp]\nlisten = \"127.0.0.1:0\"\n[webrtc]\nlisten = \"127.0.0.1:0\"\n\
             [rtmp]\nlisten = \"127.0.0.1:0\"\n[http]\nlisten = \"{api}\"\n\
             [record]\ndir = {:?}\nsegment = \"1s\"\n",
            root.join("recordings").display().to_string()
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_relay-pp"))
        .arg(&config)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the binary starts");
    wait_for("its API", || std::net::TcpStream::connect(api).is_ok());
    let (status, _, body) = http(
        api,
        "POST",
        "/api/v1/channels",
        Some(&channel("cam1", &format!("rtsp://{camera}/cam"))),
    );
    assert_eq!(status, 201, "{body}");
    let (status, _, body) = http(api, "POST", "/api/v1/channels/cam1/record", None);
    assert!((200..300).contains(&status), "{status} {body}");
    (child, api)
}

/// Sends `signal` to the binary recording, and sees it stop as it should.
fn stopped_by(signal: &str) {
    let camera = Camera::start("127.0.0.1:0".parse().unwrap());
    let root = scratch(&format!("stop_{signal}"));
    let (mut child, _api) = recording(&root, camera.addr);
    wait_for("a segment recorded", || {
        playlists(&root).iter().any(|list| list.contains("#EXTINF"))
    });
    let sent = Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let asked = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if asked.elapsed() > STOPS_WITHIN {
            let _ = child.kill();
            panic!("still running {STOPS_WITHIN:?} after SIG{signal}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "SIG{signal}: {status}");
    let lists = playlists(&root);
    assert!(!lists.is_empty());
    for list in &lists {
        assert!(list.contains("#EXT-X-ENDLIST"), "not ended:\n{list}");
    }
    camera.stop();
    let _ = std::fs::remove_dir_all(&root);
}

/// What systemd, `docker stop` and `kill` send.
#[test]
fn sigterm_finishes_the_recording_and_exits() {
    stopped_by("TERM");
}

/// Ctrl+C in a terminal.
#[test]
fn sigint_finishes_the_recording_and_exits() {
    stopped_by("INT");
}
