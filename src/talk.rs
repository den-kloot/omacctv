//! Two-way audio: the phone's microphone (system ffmpeg, pulse source "default") as
//! G.711 mu-law 8 kHz, either straight to the camera over its ONVIF RTSP backchannel
//! (camera with `talk_url`) or over TCP to omacctv-talk-relay next to Frigate, which keeps
//! the camera passwords and passes it on. While talking, the camera sound on the phone is muted
//! so the speaker doesn't feed back into the microphone.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum Event {
    Talking,
    Ended(Option<String>),
}

pub struct Talk {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Drop for Talk {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Mutes or unmutes this app's own sound streams (pulse application name "OmaCCTV").
pub fn mute_own_sound(mute: bool) {
    let Ok(out) = Command::new("pactl").args(["-f", "json", "list", "sink-inputs"]).output() else { return };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return };
    for si in v.as_array().into_iter().flatten() {
        if si["properties"]["application.name"].as_str() == Some("OmaCCTV") {
            if let Some(id) = si["index"].as_u64() {
                let _ = Command::new("pactl")
                    .args(["set-sink-input-mute", &id.to_string(), if mute { "1" } else { "0" }])
                    .status();
            }
        }
    }
}

impl Talk {
    pub fn start(relay: String, camera: crate::config::Camera, tx: async_channel::Sender<Event>) -> Talk {
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let (stop2, child2) = (stop.clone(), child.clone());
        std::thread::spawn(move || {
            let r = run(&relay, &camera, &stop2, &child2, &tx);
            mute_own_sound(false);
            let _ = tx.send_blocking(Event::Ended(r.err()));
        });
        Talk { stop, child }
    }
}

enum Sink {
    Relay(TcpStream),
    Direct(crate::rtsp::Backchannel),
}

impl Sink {
    fn send(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Relay(s) => s.write_all(b),
            Sink::Direct(bc) => bc.send(b),
        }
    }
}

fn connect_relay(relay: &str, camera: &str) -> Result<TcpStream, String> {
    let addr = relay.to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no relay address")?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(4))
        .map_err(|_| "Talk relay not reachable".to_string())?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(8))).ok();
    s.write_all(format!("TALK {camera}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    let mut b = [0u8; 1];
    while !reply.ends_with(b"\n") && reply.len() < 200 {
        match s.read(&mut b) {
            Ok(1) => reply.push(b[0]),
            _ => break,
        }
    }
    let reply = String::from_utf8_lossy(&reply).trim().to_string();
    if reply != "OK" {
        return Err(match reply.as_str() {
            "ERR busy" => "Someone else is already talking".into(),
            "ERR no speaker" => "This camera has no speaker".into(),
            "" => "No answer from the talk relay".into(),
            r => r.trim_start_matches("ERR ").to_string(),
        });
    }
    Ok(s)
}

fn run(
    relay: &str,
    camera: &crate::config::Camera,
    stop: &AtomicBool,
    child: &Mutex<Option<Child>>,
    tx: &async_channel::Sender<Event>,
) -> Result<(), String> {
    let mut s = match &camera.talk_url {
        Some(u) => Sink::Direct(crate::rtsp::Backchannel::open(u)?),
        None => Sink::Relay(connect_relay(relay, &camera.id)?),
    };
    if stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut c = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-fflags", "nobuffer", "-f", "pulse", "-fragment_size", "640"])
        .args(["-i", "default", "-af", "highpass=f=150,lowpass=f=3400,volume=2.0", "-ac", "1", "-ar", "8000"])
        .args(["-f", "mulaw", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Microphone: {e}"))?;
    let mut out = c.stdout.take().ok_or("Microphone")?;
    *child.lock().unwrap() = Some(c);
    mute_own_sound(true);
    let _ = tx.send_blocking(Event::Talking);
    let mut buf = [0u8; 320];
    let mut total = 0usize;
    while !stop.load(Ordering::SeqCst) {
        let n = out.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            if total == 0 && !stop.load(Ordering::SeqCst) {
                return Err("Microphone not available".into());
            }
            break;
        }
        total += n;
        if s.send(&buf[..n]).is_err() {
            return Err("Lost the connection to the speaker".into());
        }
    }
    Ok(())
}
