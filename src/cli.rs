//! Command line helpers, made for setting the app up by hand or by an agent:
//! `config` prints the effective config, `discover` builds one from Frigate,
//! `check` tests the config against Frigate, go2rtc and the talk relay.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use crate::config::{self, Config};

pub const HELP: &str = "\
OmaCCTV: all Frigate cameras live, with a timeline through the recordings.

Usage: omacctv [--config PATH] [COMMAND]

Without a command the app window opens.

Commands:
  list                     the configured cameras, one per line
  add SOURCE [OPTIONS]     add a camera and probe it (resolution, codec, two-way audio):
                             SOURCE = a Frigate camera name (live + recordings), or any
                             stream URL ffmpeg plays (rtsp://user:pass@cam/..., live only)
                             --name NAME  --id ID  --full URL (bigger stream, single view)
                             --talk-url URL (camera's own rtsp URL, to test two-way audio)
                             --talk (use the talk relay)  --no-talk  --span 1|2  --heavy
  remove ID|URL            remove a camera
  scan [NET] [--json]      look for cameras on the network: ONVIF discovery, RTSP 554,
                           go2rtc 8554/1984, Frigate 5000 (NET default: this /24)
  config [--raw]           print the effective config (file + discovered cameras) as JSON;
                           passwords in URLs are hidden unless --raw
  discover [URL] [--write] build a config from Frigate's /api/config (URL default: the
                           config's frigate); --write saves it to the config path
  check                    test the config: Frigate, every camera, go2rtc streams, talk relay
  probe [SECONDS] [vod]    decode every camera headless and print first-frame time and fps
  help                     this text

Config file (first that exists): --config PATH, $OMACCTV_CONFIG,
$XDG_CONFIG_HOME/omacctv/config.json, ~/.config/omacctv/config.json.
Environment: OMACCTV_DEBUG=1 shows ffmpeg's output.
";

fn redact_json(v: &mut Value) {
    match v {
        Value::String(s) if s.starts_with("rtsp://") => *s = crate::rtsp::redact(s),
        Value::Array(a) => a.iter_mut().for_each(redact_json),
        Value::Object(o) => o.values_mut().for_each(redact_json),
        _ => {}
    }
}

pub fn config(args: &[String]) -> i32 {
    let cfg = Config::load();
    eprintln!(
        "# {}{}",
        cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or("no config file, built-in defaults".into()),
        if cfg.discovered { ", cameras discovered from Frigate" } else { "" }
    );
    let mut v = cfg.to_json();
    if !args.iter().any(|a| a == "--raw") {
        redact_json(&mut v);
    }
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    0
}

pub fn list() -> i32 {
    let cfg = Config::load();
    if cfg.cameras.is_empty() {
        println!("no cameras (omacctv add …, omacctv scan)");
        return 1;
    }
    for c in &cfg.cameras {
        let src = match &c.url {
            Some(u) => crate::rtsp::redact(u),
            None => format!("frigate:{} (go2rtc {} / {})", c.id, c.live_grid, c.live_full),
        };
        let talk = if !c.talk {
            "".into()
        } else if c.talk_url.is_some() {
            "  talk: direct".into()
        } else if cfg.talk_relay.is_empty() {
            "  talk: needs talk_relay".to_string()
        } else {
            "  talk: relay".into()
        };
        println!("{:<20} {:<18} {src}{talk}", c.id, c.name);
    }
    if cfg.discovered {
        println!("(cameras discovered from Frigate; the config file lists none)");
    }
    0
}

fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Video size and codec via ffprobe (killed after 15 s).
fn ffprobe(url: &str) -> Result<(i64, i64, String), String> {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.args(["-v", "error"]);
    if url.starts_with("rtsp") {
        cmd.args(["-rtsp_transport", "tcp"]);
    }
    cmd.args(["-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height", "-of", "json", url]);
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).stdin(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("ffprobe: {e}"))?;
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t0.elapsed() > Duration::from_secs(15) => {
                let _ = child.kill();
                return Err("ffprobe: no answer in 15 s".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let st = &v["streams"][0];
    match (st["width"].as_i64(), st["height"].as_i64()) {
        (Some(w), Some(h)) if h > 0 => Ok((w, h, st["codec_name"].as_str().unwrap_or("?").to_string())),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("no video stream").to_string();
            Err(format!("ffprobe: {}", crate::rtsp::redact(&err)))
        }
    }
}

/// Reads the config file as JSON (or a new one), lets `f` change the camera list, writes it.
fn edit_cameras(f: impl FnOnce(&mut Vec<Value>, &Config) -> Result<String, String>) -> i32 {
    let cfg = Config::load();
    let path = cfg.path.clone().unwrap_or_else(config::default_path);
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(t) => match serde_json::from_str(&t) {
            Ok(v @ Value::Object(_)) => v,
            _ => {
                eprintln!("omacctv: {} is not a JSON object; fix it first (omacctv check)", path.display());
                return 1;
            }
        },
        Err(_) => serde_json::json!({}),
    };
    let mut cams: Vec<Value> = match root.get("cameras").and_then(Value::as_array) {
        Some(a) => a.clone(),
        // No list yet: start from what was discovered so those cameras stay.
        None => cfg.cameras.iter().map(config::camera_json).collect(),
    };
    let msg = match f(&mut cams, &cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("omacctv: {e}");
            return 1;
        }
    };
    root["cameras"] = Value::from(cams);
    let secret = root.to_string().contains("@") && root.to_string().contains("rtsp://");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(&root).unwrap_or_default() + "\n";
    if let Err(e) = std::fs::write(&path, text) {
        eprintln!("omacctv: {}: {e}", path.display());
        return 1;
    }
    if secret {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("{msg}");
    println!("saved {}{}", path.display(), if secret { " (mode 600: it holds camera passwords)" } else { "" });
    println!("restart the app to see it");
    0
}

pub fn add(args: &[String]) -> i32 {
    let flags_with_value = ["--name", "--id", "--full", "--talk-url", "--span"];
    let mut source = None;
    let mut i = 0;
    while i < args.len() {
        if flags_with_value.contains(&args[i].as_str()) {
            i += 2;
            continue;
        }
        if !args[i].starts_with("--") && source.is_none() {
            // go2rtc-style options after '#' (e.g. #backchannel=0) mean nothing to ffmpeg.
            let a = &args[i];
            source = Some(if a.contains("://") { a.split('#').next().unwrap_or(a).to_string() } else { a.clone() });
        }
        i += 1;
    }
    let Some(source) = source else {
        eprintln!("usage: omacctv add SOURCE [--name NAME] [--id ID] [--full URL] [--talk-url URL] [--talk|--no-talk] [--span 1|2] [--heavy]");
        return 2;
    };
    let has = |f: &str| args.iter().any(|a| a == f);
    edit_cameras(|cams, cfg| {
        let mut notes = Vec::new();
        let mut cam = if source.contains("://") {
            let host = crate::rtsp::Url::parse(&source).map(|u| u.host).unwrap_or_else(|_| "camera".into());
            let name = opt(args, "--name").map(String::from).unwrap_or_else(|| host.clone());
            let id = opt(args, "--id").map(String::from).unwrap_or_else(|| slug(&name));
            config::Camera {
                id,
                name,
                live_grid: String::new(),
                live_full: String::new(),
                span: 1,
                aspect: 16.0 / 9.0,
                heavy_record: false,
                talk: false,
                url: Some(source.clone()),
                url_full: opt(args, "--full").map(String::from),
                talk_url: None,
            }
        } else {
            if !cfg.has_frigate {
                return Err("no Frigate configured: give a stream URL (rtsp://…)".into());
            }
            let all = config::discover(&cfg.frigate).map_err(|e| format!("Frigate {}: {e}", cfg.frigate))?;
            let Some(mut c) = all.iter().find(|c| c.id == source).cloned() else {
                let ids: Vec<&str> = all.iter().map(|c| c.id.as_str()).collect();
                return Err(format!("Frigate has no camera \"{source}\" (it has: {})", ids.join(", ")));
            };
            if let Some(n) = opt(args, "--name") {
                c.name = n.into();
            }
            if let Some(id) = opt(args, "--id") {
                notes.push(format!("--id ignored: a Frigate camera keeps its Frigate name ({})", c.id));
                let _ = id;
            }
            c
        };
        if cam.id.is_empty() {
            return Err("give the camera an --id".into());
        }
        // Size and codec of the stream the single view shows.
        let probe_url = cam.live_url(&cfg.rtsp, true);
        match ffprobe(&probe_url) {
            Ok((w, h, codec)) => {
                cam.aspect = w as f64 / h as f64;
                cam.span = if cam.aspect > 2.2 { 2 } else { 1 };
                cam.heavy_record = has("--heavy") || (codec == "hevc" && w * h > 4_000_000);
                notes.push(format!("video: {w}x{h} {codec}"));
                if codec == "hevc" && w * h > 4_000_000 && cam.url.is_none() && cam.live_full != cam.live_grid {
                    notes.push(format!(
                        "main stream is big H.265, too heavy for a phone live: the single view uses {} too",
                        cam.live_grid
                    ));
                    cam.live_full = cam.live_grid.clone();
                }
            }
            Err(e) => notes.push(format!("could not probe the video ({e}); tile shape guessed 16:9")),
        }
        if let Some(sp) = opt(args, "--span").and_then(|s| s.parse::<i32>().ok()) {
            cam.span = sp.clamp(1, 2);
        }
        // Two-way audio: ask the camera itself for its ONVIF backchannel.
        let talk_probe = opt(args, "--talk-url")
            .map(String::from)
            .or_else(|| cam.url.as_ref().filter(|u| u.starts_with("rtsp://")).map(|_| cam.live_url("", true)));
        if has("--no-talk") {
            notes.push("two-way audio: off (--no-talk)".into());
        } else if let Some(u) = talk_probe {
            match crate::rtsp::probe(&u) {
                Ok(info) if info.backchannel.is_some() => {
                    cam.talk = true;
                    cam.talk_url = Some(u.clone());
                    notes.push("two-way audio: yes (ONVIF backchannel, PCMU) → talking goes straight to the camera".into());
                }
                Ok(info) => {
                    let why = match (&info.backchannel_other, info.server.to_lowercase().contains("go2rtc")) {
                        (Some(c), _) => format!("backchannel is {c}, only PCMU is supported"),
                        (None, true) => "this is a go2rtc restream, it does not pass the backchannel on; use --talk-url with the camera's own rtsp URL".into(),
                        (None, false) => "the camera offers no ONVIF backchannel, so no speaker".into(),
                    };
                    notes.push(format!("two-way audio: no ({why})"));
                }
                Err(e) => notes.push(format!("two-way audio: could not ask the camera ({e})")),
            }
        } else if has("--talk") {
            cam.talk = true;
            notes.push(if cfg.talk_relay.is_empty() {
                "two-way audio: on, but talk_relay is empty: set it in the config".into()
            } else {
                format!("two-way audio: through the talk relay {} (it needs TALK_URL_{})", cfg.talk_relay, cam.id.replace('-', "_"))
            });
        } else {
            notes.push("two-way audio: not tested; Frigate/go2rtc does not tell. Pass --talk-url rtsp://USER:PASS@CAMERA/… to test it, or --talk to use the talk relay".into());
        }
        let v = config::camera_json(&cam);
        let verb = match cams.iter().position(|c| c["id"].as_str() == Some(cam.id.as_str())) {
            Some(i) => {
                cams[i] = v;
                "updated"
            }
            None => {
                cams.push(v);
                "added"
            }
        };
        let mut msg = format!("{verb} camera {} ({})", cam.id, cam.name);
        for n in notes {
            msg.push_str(&format!("\n  {n}"));
        }
        Ok(msg)
    })
}

pub fn remove(args: &[String]) -> i32 {
    let Some(what) = args.first().cloned() else {
        eprintln!("usage: omacctv remove ID|URL");
        return 2;
    };
    edit_cameras(|cams, _| {
        let hit = |c: &Value| {
            ["id", "url", "url_full", "talk_url"].iter().any(|k| {
                c[*k].as_str().is_some_and(|v| v == what || crate::rtsp::redact(v) == what)
            })
        };
        let before = cams.len();
        let gone: Vec<String> = cams.iter().filter(|c| hit(c)).filter_map(|c| c["id"].as_str().map(String::from)).collect();
        cams.retain(|c| !hit(c));
        if cams.len() == before {
            let ids: Vec<&str> = cams.iter().filter_map(|c| c["id"].as_str()).collect();
            return Err(format!("no camera \"{what}\" (there are: {})", ids.join(", ")));
        }
        Ok(format!("removed {}", gone.join(", ")))
    })
}

pub fn discover(args: &[String]) -> i32 {
    let write = args.iter().any(|a| a == "--write");
    let base = Config::load();
    let url = args.iter().find(|a| !a.starts_with("--")).map(|s| s.trim_end_matches('/').to_string());
    let mut cfg = match &url {
        Some(u) => {
            let mut c = Config::parse(&format!("{{\"frigate\": {}}}", Value::from(u.as_str()))).unwrap_or_default();
            c.talk_relay = base.talk_relay.clone();
            c
        }
        None => base,
    };
    match config::discover(&cfg.frigate) {
        Ok(c) if c.is_empty() => {
            eprintln!("omacctv: Frigate at {} has no enabled cameras", cfg.frigate);
            return 1;
        }
        Ok(c) => cfg.cameras = c,
        Err(e) => {
            eprintln!("omacctv: {}/api/config: {e}", cfg.frigate);
            return 1;
        }
    }
    let text = serde_json::to_string_pretty(&cfg.to_json()).unwrap_or_default() + "\n";
    if write {
        let path = config::default_path();
        if path.exists() {
            eprintln!("omacctv: {} exists, not overwriting (print without --write and merge by hand)", path.display());
            return 1;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, &text) {
            eprintln!("omacctv: {}: {e}", path.display());
            return 1;
        }
        eprintln!("wrote {}", path.display());
    } else {
        print!("{text}");
    }
    0
}

fn tcp_ok(hostport: &str) -> Result<(), String> {
    let sa = hostport.to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no address")?;
    TcpStream::connect_timeout(&sa, Duration::from_secs(3)).map(|_| ()).map_err(|e| e.to_string())
}

fn hostport(url: &str, default_port: u16) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let hp = rest.split('/').next().unwrap_or(rest);
    if hp.contains(':') { hp.to_string() } else { format!("{hp}:{default_port}") }
}

pub fn check() -> i32 {
    let cfg = Config::load();
    let mut bad = 0;
    let mut ok = |good: bool, what: String| {
        println!("{} {what}", if good { "ok  " } else { "FAIL" });
        if !good {
            bad += 1;
        }
    };
    println!(
        "config: {}",
        cfg.path.as_ref().map(|p| p.display().to_string()).unwrap_or("none (built-in defaults)".into())
    );
    for w in &cfg.warnings {
        ok(false, format!("config: {w}"));
    }
    let fcfg: Option<Value> = if !cfg.has_frigate {
        println!("frigate: none configured (live-only cameras)");
        None
    } else {
        match crate::frigate::get(&format!("{}/api/config", cfg.frigate), Duration::from_secs(8)) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(v) => {
                ok(true, format!("frigate {} answers", cfg.frigate));
                Some(v)
            }
            Err(e) => {
                ok(false, format!("frigate {}/api/config is not JSON: {e}", cfg.frigate));
                None
            }
        },
        Err(e) => {
            ok(false, format!("frigate {}: {e}", cfg.frigate));
            None
        }
    }
    };
    if cfg.cameras.iter().any(|c| c.recorded()) {
        let rtsp = hostport(&cfg.rtsp, 554);
        ok(tcp_ok(&rtsp).is_ok(), format!("go2rtc rtsp {rtsp} reachable"));
    }
    ok(!cfg.cameras.is_empty(), format!("{} camera(s){}", cfg.cameras.len(), if cfg.discovered { " (discovered)" } else { "" }));
    if let Some(v) = &fcfg {
        let streams = v["go2rtc"]["streams"].as_object();
        for c in &cfg.cameras {
            if !c.recorded() {
                continue;
            }
            ok(v["cameras"].get(&c.id).is_some(), format!("camera {}: known to Frigate", c.id));
            if let Some(s) = streams {
                if !c.recorded() {
                    continue;
                }
                let mut want = vec![&c.live_grid];
                if c.live_full != c.live_grid {
                    want.push(&c.live_full);
                }
                for st in want {
                    ok(s.contains_key(st.as_str()), format!("camera {}: go2rtc stream {st}", c.id));
                }
            }
        }
    }
    for c in cfg.cameras.iter().filter(|c| !c.recorded()) {
        let u = c.live_url(&cfg.rtsp, false);
        let r = if u.starts_with("rtsp://") { crate::rtsp::probe(&u).map(|_| ()) } else { ffprobe(&u).map(|_| ()) };
        ok(r.is_ok(), format!("camera {}: {} {}", c.id, crate::rtsp::redact(&u), r.err().unwrap_or("answers".into())));
    }
    for c in cfg.cameras.iter().filter(|c| c.talk_url.is_some()) {
        let r = crate::rtsp::probe(c.talk_url.as_deref().unwrap_or(""));
        let good = matches!(&r, Ok(i) if i.backchannel.is_some());
        ok(good, format!("camera {}: two-way audio (backchannel) {}", c.id, match r { Ok(_) if good => "ok".into(), Ok(_) => "not offered".into(), Err(e) => e }));
    }
    let talkers = cfg.cameras.iter().filter(|c| c.talk && c.talk_url.is_none()).count();
    if cfg.talk_relay.is_empty() {
        if talkers > 0 {
            ok(false, format!("{talkers} camera(s) have \"talk\": true but neither talk_url nor talk_relay"));
        }
    } else if talkers > 0 {
        ok(tcp_ok(&cfg.talk_relay).is_ok(), format!("talk relay {} reachable", cfg.talk_relay));
    }
    for tool in ["ffmpeg"] {
        let found = std::process::Command::new(tool).arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
        ok(found, format!("{tool} installed"));
    }
    if bad == 0 {
        println!("all good");
        0
    } else {
        println!("{bad} problem(s)");
        1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hostport() {
        assert_eq!(super::hostport("rtsp://a:8554", 554), "a:8554");
        assert_eq!(super::hostport("rtsp://a/x", 554), "a:554");
    }
}
