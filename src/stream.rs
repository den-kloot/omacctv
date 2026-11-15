//! One video stream = one ffmpeg child (system ffmpeg: multithreaded NEON decoders for
//! H.264 and H.265, RTSP and HLS) writing scaled RGB frames as PPM to a pipe. A reader
//! thread keeps only the newest frame in a slot; the UI picks it up on the next frame tick.

use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Frame {
    pub width: i32,
    pub height: i32,
    pub rgb: Vec<u8>,
}

#[derive(Default)]
pub struct SlotInner {
    pub frame: Option<Frame>,
    /// Frames received so far (only grows).
    pub count: u64,
    /// A JPEG (scrub preview) with its request id.
    pub jpeg: Option<(u64, Vec<u8>)>,
}

pub type Slot = Arc<Mutex<SlotInner>>;

pub struct Stream {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            std::thread::spawn(move || {
                let _ = c.wait();
            });
        }
    }
}

fn scale(width: i32) -> String {
    format!("scale=w='min({width},iw)':h=-2:flags=bilinear")
}

/// Playback speeds offered next to the play button.
pub const RATES: [f64; 6] = [0.5, 1.0, 2.0, 4.0, 8.0, 16.0];

/// Sound only up to this speed (time-stretched, so it stays in step with the picture).
pub const MAX_AUDIO_RATE: f64 = 2.0;

/// "0.5×", "1×", "16×".
pub fn rate_label(rate: f64) -> String {
    if rate.fract() == 0.0 { format!("{rate:.0}×") } else { format!("{rate}×") }
}

/// Outputs: scaled PPM frames on stdout, and with `audio` the first sound track to PipeWire
/// (pulse) from the same process, so picture and sound stay together. `tempo` != 1 stretches
/// the sound to the playback speed (pulse plays in real time and would otherwise hold ffmpeg back).
fn outputs(a: &mut Vec<String>, width: i32, audio: bool, tempo: f64) {
    a.extend(["-map", "0:v:0", "-an", "-fps_mode", "passthrough", "-vf"].map(String::from));
    a.push(scale(width));
    a.extend(["-f", "image2pipe", "-c:v", "ppm", "-"].map(String::from));
    if audio {
        a.extend(
            ["-map", "0:a:0?", "-vn", "-ac", "2", "-ar", "48000"].map(String::from),
        );
        if tempo != 1.0 {
            a.extend(["-af".into(), format!("atempo={tempo}")]);
        }
        a.extend(["-f", "pulse", "-name", "OmaCCTV", "-stream_name", "Camera", "default"].map(String::from));
    }
}

/// ffmpeg arguments for a live RTSP stream (go2rtc restream).
pub fn live_args(url: &str, width: i32, audio: bool) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-fflags", "nobuffer", "-flags", "low_delay"]
        .map(String::from)
        .to_vec();
    a.extend(["-probesize", "500000", "-analyzeduration", "500000"].map(String::from));
    if url.starts_with("rtsp") {
        a.extend(["-rtsp_transport", "tcp"].map(String::from));
    }
    a.push("-i".into());
    a.push(url.into());
    outputs(&mut a, width, audio, 1.0);
    a
}

/// ffmpeg arguments for a recording (HLS from Frigate), paced at `rate` × real time.
/// Fast playback decodes only key frames (a phone can't decode 4+ streams at 8×), heavy
/// cameras already from 2×; sound only up to MAX_AUDIO_RATE.
pub fn vod_args(url: &str, width: i32, heavy: bool, audio: bool, rate: f64) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"].map(String::from).to_vec();
    if heavy {
        a.extend(["-skip_loop_filter", "all"].map(String::from));
    }
    if rate >= 4.0 || (heavy && rate >= 2.0) {
        a.extend(["-skip_frame", "nokey"].map(String::from));
    }
    a.extend(["-readrate".into(), format!("{rate}"), "-i".into()]);
    a.push(url.into());
    outputs(&mut a, width, audio && rate <= MAX_AUDIO_RATE, rate);
    a
}

/// Reads one PPM header token (skipping whitespace); `None` at end of stream.
fn token(r: &mut impl Read) -> Option<String> {
    let mut out = String::new();
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b).ok()? == 0 {
            return None;
        }
        let c = b[0];
        if c.is_ascii_whitespace() {
            if out.is_empty() {
                continue;
            }
            return Some(out);
        }
        out.push(c as char);
        if out.len() > 16 {
            return None;
        }
    }
}

fn read_frames(r: &mut impl Read, slot: &Slot, stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let (Some(magic), Some(w), Some(h), Some(_max)) = (token(r), token(r), token(r), token(r)) else { return };
        let (Ok(w), Ok(h)) = (w.parse::<i32>(), h.parse::<i32>()) else { return };
        if magic != "P6" || w <= 0 || h <= 0 || w > 8192 || h > 8192 {
            return;
        }
        let mut rgb = vec![0u8; (w * h * 3) as usize];
        if r.read_exact(&mut rgb).is_err() {
            return;
        }
        let mut s = slot.lock().unwrap();
        s.frame = Some(Frame { width: w, height: h, rgb });
        s.count += 1;
    }
}

impl Stream {
    /// Starts ffmpeg with `args`. `restart` = start again when it ends (live streams).
    pub fn spawn(args: Vec<String>, slot: Slot, restart: bool) -> Stream {
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let (stop2, child2) = (stop.clone(), child.clone());
        std::thread::spawn(move || {
            let mut backoff = 1;
            while !stop2.load(Ordering::SeqCst) {
                let spawned = Command::new("ffmpeg")
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(if std::env::var_os("OMACCTV_DEBUG").is_some() { Stdio::inherit() } else { Stdio::null() })
                    .spawn();
                let mut c = match spawned {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("ffmpeg: {e}");
                        return;
                    }
                };
                let out = c.stdout.take();
                {
                    let mut g = child2.lock().unwrap();
                    if stop2.load(Ordering::SeqCst) {
                        let _ = c.kill();
                        let _ = c.wait();
                        return;
                    }
                    *g = Some(c);
                }
                let before = slot.lock().unwrap().count;
                if let Some(out) = out {
                    read_frames(&mut BufReader::with_capacity(1 << 20, out), &slot, &stop2);
                }
                if let Some(mut c) = child2.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                if !restart {
                    return;
                }
                // Got pictures = it was working: retry fast; otherwise back off up to 8 s.
                backoff = if slot.lock().unwrap().count > before { 1 } else { (backoff * 2).min(8) };
                for _ in 0..backoff * 10 {
                    if stop2.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Stream { stop, child }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ppm_frames() {
        let mut data = b"P6\n2 1\n255\n".to_vec();
        data.extend([1, 2, 3, 4, 5, 6]);
        data.extend(b"P6\n1 1\n255\n");
        data.extend([7, 8, 9]);
        let slot: Slot = Default::default();
        read_frames(&mut &data[..], &slot, &AtomicBool::new(false));
        let s = slot.lock().unwrap();
        assert_eq!(s.count, 2);
        assert_eq!(s.frame.as_ref().unwrap().rgb, vec![7, 8, 9]);
    }

    #[test]
    fn vod_speed_args() {
        let a = vod_args("u", 640, false, true, 1.0);
        assert!(a.windows(2).any(|w| w == ["-readrate", "1"]));
        assert!(!a.iter().any(|x| x == "nokey" || x.starts_with("atempo")));
        let a = vod_args("u", 640, false, true, 2.0);
        assert!(a.iter().any(|x| x == "atempo=2") && !a.iter().any(|x| x == "nokey"));
        let a = vod_args("u", 640, false, true, 0.5);
        assert!(a.windows(2).any(|w| w == ["-readrate", "0.5"]) && a.iter().any(|x| x == "atempo=0.5"));
        let a = vod_args("u", 640, false, true, 16.0);
        assert!(a.iter().any(|x| x == "nokey") && !a.iter().any(|x| x == "pulse"));
        assert!(vod_args("u", 640, true, false, 2.0).iter().any(|x| x == "nokey"));
        assert_eq!(rate_label(0.5), "0.5×");
        assert_eq!(rate_label(16.0), "16×");
    }
}
