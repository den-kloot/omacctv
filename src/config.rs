//! Configuration: one JSON file (see `examples/config.json` and the README).
//!
//! Lookup order: `--config PATH` / `-c PATH`, `$OMACCTV_CONFIG`, `$XDG_CONFIG_HOME/omacctv/config.json`,
//! `~/.config/omacctv/config.json`, and the old `~/.config/kloot-multiview/config.json`.
//! Every key is optional. Without a `cameras` list the cameras are read from Frigate itself
//! (`/api/config`), so `{"frigate": "http://HOST:5000"}` is a complete config.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Camera {
    /// Frigate camera name (recordings, events, snapshots).
    pub id: String,
    /// Shown on the tile.
    pub name: String,
    /// go2rtc stream for the grid (small) and for the single view (big).
    pub live_grid: String,
    pub live_full: String,
    /// 1 = half width in the grid, 2 = full width.
    pub span: i32,
    /// Picture width / height (tile shape before the first frame arrives).
    pub aspect: f64,
    /// Recordings are heavy (e.g. 5K H.265): decode them with the loop filter skipped.
    pub heavy_record: bool,
    /// Has a speaker: the talk button is shown.
    pub talk: bool,
    /// A stream URL of its own (rtsp://, http://, anything ffmpeg plays) instead of a go2rtc
    /// stream name: live only, no recordings (the camera is not in Frigate).
    pub url: Option<String>,
    /// Bigger stream for the single view (default: `url`).
    pub url_full: Option<String>,
    /// The camera's own RTSP URL for two-way audio over its ONVIF backchannel, straight from
    /// this app; without it talking goes through `talk_relay`.
    pub talk_url: Option<String>,
}

impl Camera {
    /// Frigate records it (recordings, timeline, snapshots).
    pub fn recorded(&self) -> bool {
        self.url.is_none()
    }

    /// The live stream for the grid (small) or the single view.
    pub fn live_url(&self, rtsp: &str, full: bool) -> String {
        match (&self.url, &self.url_full) {
            (Some(_), Some(f)) if full => f.clone(),
            (Some(u), _) => u.clone(),
            _ => format!("{rtsp}/{}", if full { &self.live_full } else { &self.live_grid }),
        }
    }

    /// Talking is possible with this config.
    pub fn can_talk(&self, relay: &str) -> bool {
        self.talk && (self.talk_url.is_some() || !relay.is_empty())
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Frigate base URL (plain http, LAN or VPN).
    pub frigate: String,
    /// go2rtc RTSP base URL; default: Frigate's host, port 8554.
    pub rtsp: String,
    /// host:port of omacctv-talk-relay; empty = no talking.
    pub talk_relay: String,
    pub cameras: Vec<Camera>,
    /// A Frigate is configured (or none is said: the default 127.0.0.1:5000 is tried).
    pub has_frigate: bool,
    /// The file that was read (None = built-in defaults).
    pub path: Option<PathBuf>,
    /// The cameras came from Frigate, not from the file.
    pub discovered: bool,
    /// Problems found while loading (shown by `omacctv check`, printed to stderr).
    pub warnings: Vec<String>,
}

const KEYS: &[&str] = &["frigate", "rtsp", "talk_relay", "cameras"];
const CAM_KEYS: &[&str] =
    &["id", "name", "live_grid", "live_full", "span", "aspect", "heavy_record", "talk", "url", "url_full", "talk_url"];

impl Default for Config {
    fn default() -> Self {
        Config {
            frigate: "http://127.0.0.1:5000".into(),
            rtsp: String::new(),
            talk_relay: String::new(),
            cameras: Vec::new(),
            has_frigate: true,
            path: None,
            discovered: false,
            warnings: Vec::new(),
        }
    }
}

/// `rtsp://HOST:8554` for `http://HOST:5000`.
fn rtsp_from(frigate: &str) -> String {
    let rest = frigate.split("://").nth(1).unwrap_or(frigate);
    let host = rest.split(['/', ':']).next().unwrap_or("127.0.0.1");
    format!("rtsp://{host}:8554")
}

/// The config file to read: the first candidate that exists.
pub fn find_path() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--config" || a == "-c") {
        return args.get(i + 1).map(PathBuf::from);
    }
    if let Some(p) = std::env::var_os("OMACCTV_CONFIG") {
        return Some(p.into());
    }
    let mut cands = Vec::new();
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        cands.push(PathBuf::from(x).join("omacctv/config.json"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        let h = PathBuf::from(h);
        cands.push(h.join(".config/omacctv/config.json"));
        cands.push(h.join(".config/kloot-multiview/config.json"));
    }
    cands.into_iter().find(|p| p.exists())
}

/// Where a new config file goes (`omacctv discover --write`).
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("OMACCTV_CONFIG") {
        return p.into();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| ".".into());
    base.join("omacctv/config.json")
}

fn camera(c: &Value, warnings: &mut Vec<String>) -> Option<Camera> {
    let Some(id) = c["id"].as_str() else {
        warnings.push(format!("camera without \"id\" ignored: {c}"));
        return None;
    };
    if let Some(o) = c.as_object() {
        for k in o.keys().filter(|k| !k.starts_with('_') && !CAM_KEYS.contains(&k.as_str())) {
            warnings.push(format!("camera {id}: unknown key \"{k}\""));
        }
    }
    let grid = c["live_grid"].as_str().unwrap_or(id);
    let span = c["span"].as_i64().unwrap_or(1);
    if !(1..=2).contains(&span) {
        warnings.push(format!("camera {id}: span must be 1 or 2"));
    }
    Some(Camera {
        id: id.into(),
        name: c["name"].as_str().unwrap_or(id).into(),
        live_grid: grid.into(),
        live_full: c["live_full"].as_str().unwrap_or(grid).into(),
        span: span.clamp(1, 2) as i32,
        aspect: c["aspect"].as_f64().filter(|a| *a > 0.1).unwrap_or(16.0 / 9.0),
        heavy_record: c["heavy_record"].as_bool().unwrap_or(false),
        talk: c["talk"].as_bool().unwrap_or(c["talk_url"].is_string()),
        url: c["url"].as_str().map(String::from),
        url_full: c["url_full"].as_str().map(String::from),
        talk_url: c["talk_url"].as_str().map(String::from),
    })
}

impl Config {
    /// Parse a config text. Cameras are not discovered here.
    pub fn parse(text: &str) -> Result<Config, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        let o = v.as_object().ok_or("the config must be a JSON object")?;
        let mut cfg = Config::default();
        for k in o.keys().filter(|k| !k.starts_with('_') && !KEYS.contains(&k.as_str())) {
            cfg.warnings.push(format!("unknown key \"{k}\""));
        }
        match &v["frigate"] {
            Value::String(s) if !s.is_empty() => cfg.frigate = s.trim_end_matches('/').into(),
            // "frigate": "" or null = no Frigate: only cameras with their own url.
            Value::String(_) | Value::Null if o.contains_key("frigate") => cfg.has_frigate = false,
            _ => {}
        }
        if cfg.has_frigate && !cfg.frigate.starts_with("http://") {
            cfg.warnings.push(format!("frigate must be an http:// URL (got {})", cfg.frigate));
        }
        cfg.rtsp = match v["rtsp"].as_str() {
            Some(s) => s.trim_end_matches('/').into(),
            None => rtsp_from(&cfg.frigate),
        };
        if let Some(s) = v["talk_relay"].as_str() {
            cfg.talk_relay = s.into();
        }
        if let Some(list) = v["cameras"].as_array() {
            let mut w = Vec::new();
            cfg.cameras = list.iter().filter_map(|c| camera(c, &mut w)).collect();
            cfg.warnings.extend(w);
        }
        Ok(cfg)
    }

    /// Read the config file (if any); without cameras, ask Frigate for them.
    pub fn load() -> Config {
        let path = find_path();
        let mut cfg = match &path {
            Some(p) if !p.exists() => Config { rtsp: rtsp_from("http://127.0.0.1:5000"), ..Config::default() },
            None => Config { rtsp: rtsp_from("http://127.0.0.1:5000"), ..Config::default() },
            Some(p) => match std::fs::read_to_string(p).map_err(|e| e.to_string()).and_then(|t| Config::parse(&t)) {
                Ok(c) => c,
                Err(e) => {
                    let mut c = Config { rtsp: rtsp_from("http://127.0.0.1:5000"), ..Config::default() };
                    c.warnings.push(format!("{}: {e}", p.display()));
                    c
                }
            },
        };
        cfg.path = path;
        if cfg.cameras.is_empty() && cfg.has_frigate {
            match discover(&cfg.frigate) {
                Ok(c) => {
                    cfg.cameras = c;
                    cfg.discovered = true;
                }
                Err(e) => cfg.warnings.push(format!("no cameras configured and Frigate discovery failed: {e}")),
            }
        }
        for w in &cfg.warnings {
            eprintln!("omacctv: {w}");
        }
        cfg
    }

    /// The config as JSON, in the file format (all keys written out).
    pub fn to_json(&self) -> Value {
        let cams: Vec<Value> = self.cameras.iter().map(camera_json).collect();
        let mut o = Map::new();
        o.insert("frigate".into(), if self.has_frigate { self.frigate.clone() } else { String::new() }.into());
        o.insert("rtsp".into(), self.rtsp.clone().into());
        o.insert("talk_relay".into(), self.talk_relay.clone().into());
        o.insert("cameras".into(), cams.into());
        Value::Object(o)
    }
}

/// One camera in the file format; stream fields only where they mean something.
pub fn camera_json(c: &Camera) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), c.id.clone().into());
    o.insert("name".into(), c.name.clone().into());
    match &c.url {
        Some(u) => {
            o.insert("url".into(), u.clone().into());
            if let Some(f) = &c.url_full {
                o.insert("url_full".into(), f.clone().into());
            }
        }
        None => {
            o.insert("live_grid".into(), c.live_grid.clone().into());
            o.insert("live_full".into(), c.live_full.clone().into());
        }
    }
    o.insert("span".into(), c.span.into());
    o.insert("aspect".into(), json!((c.aspect * 1000.0).round() / 1000.0));
    o.insert("heavy_record".into(), c.heavy_record.into());
    o.insert("talk".into(), c.talk.into());
    if let Some(t) = &c.talk_url {
        o.insert("talk_url".into(), t.clone().into());
    }
    Value::Object(o)
}

/// Cameras from Frigate's `/api/config`: enabled cameras in Frigate's order, go2rtc streams
/// `<id>_sub` / `<id>_main` (or `<id>`) when they exist, aspect from the detect size.
pub fn discover(frigate: &str) -> Result<Vec<Camera>, String> {
    let body = crate::frigate::get(&format!("{frigate}/api/config"), Duration::from_secs(6))?;
    let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok(cameras_from_frigate(&v))
}

pub fn cameras_from_frigate(v: &Value) -> Vec<Camera> {
    let streams: Vec<&str> = v["go2rtc"]["streams"].as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    let has = |s: &str| streams.contains(&s);
    let Some(cams) = v["cameras"].as_object() else { return Vec::new() };
    cams.iter()
        .filter(|(_, c)| c["enabled"].as_bool().unwrap_or(true))
        .map(|(id, c)| {
            let pick = |a: String| has(&a).then_some(a);
            let full = pick(format!("{id}_main")).or_else(|| pick(id.clone()));
            let grid = pick(format!("{id}_sub")).or_else(|| full.clone()).unwrap_or_else(|| id.clone());
            let (w, h) = (c["detect"]["width"].as_f64().unwrap_or(16.0), c["detect"]["height"].as_f64().unwrap_or(9.0));
            let aspect = if h > 0.0 { w / h } else { 16.0 / 9.0 };
            let mut name = c["friendly_name"].as_str().map(String::from).unwrap_or_else(|| {
                let s = id.replace(['_', '-'], " ");
                let mut ch = s.chars();
                ch.next().map(|f| f.to_uppercase().collect::<String>() + ch.as_str()).unwrap_or_default()
            });
            if name.is_empty() {
                name = id.clone();
            }
            Camera {
                id: id.clone(),
                name,
                live_full: full.unwrap_or_else(|| grid.clone()),
                live_grid: grid,
                span: if aspect > 2.2 { 2 } else { 1 },
                aspect,
                heavy_record: false,
                talk: false,
                url: None,
                url_full: None,
                talk_url: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_and_full() {
        let c = Config::parse(r#"{"frigate": "http://nvr.lan:5000/"}"#).unwrap();
        assert_eq!(c.frigate, "http://nvr.lan:5000");
        assert_eq!(c.rtsp, "rtsp://nvr.lan:8554");
        assert!(c.cameras.is_empty() && c.warnings.is_empty());

        let c = Config::parse(
            r#"{"_comment": "x", "frigate": "http://a:5000", "rtsp": "rtsp://b:8554", "bogus": 1,
                "cameras": [{"id": "door", "talk": true, "span": 3, "colour": 1}, {"name": "no id"}]}"#,
        )
        .unwrap();
        assert_eq!(c.rtsp, "rtsp://b:8554");
        assert_eq!(c.cameras.len(), 1);
        let d = &c.cameras[0];
        assert_eq!((d.name.as_str(), d.live_grid.as_str(), d.live_full.as_str(), d.span, d.talk), ("door", "door", "door", 2, true));
        assert_eq!(c.warnings.len(), 4, "{:?}", c.warnings);
        assert!(Config::parse("[1]").is_err());
        assert!(Config::parse("{").is_err());
    }

    #[test]
    fn round_trip() {
        let text = include_str!("../examples/config.json");
        let c = Config::parse(text).unwrap();
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert!(!c.cameras.is_empty());
        let again = Config::parse(&c.to_json().to_string()).unwrap();
        assert_eq!(c.cameras, again.cameras);
    }

    #[test]
    fn own_urls() {
        let c = Config::parse(
            r#"{"frigate": "", "cameras": [{"id": "gate", "aspect": 1.5, "url": "rtsp://u:p@10.0.0.9/sub", "url_full": "rtsp://u:p@10.0.0.9/main",
                "talk_url": "rtsp://u:p@10.0.0.9/main"}]}"#,
        )
        .unwrap();
        assert!(!c.has_frigate && c.warnings.is_empty(), "{:?}", c.warnings);
        let g = &c.cameras[0];
        assert!(!g.recorded() && g.talk && g.can_talk(""));
        assert_eq!(g.live_url("rtsp://x", false), "rtsp://u:p@10.0.0.9/sub");
        assert_eq!(g.live_url("rtsp://x", true), "rtsp://u:p@10.0.0.9/main");
        let again = Config::parse(&c.to_json().to_string()).unwrap();
        assert_eq!(c.cameras, again.cameras);
        assert!(!again.has_frigate);
    }

    #[test]
    fn from_frigate() {
        let v: Value = serde_json::from_str(
            r#"{"go2rtc": {"streams": {"front_main": [], "front_sub": [], "garage": []}},
                "cameras": {"front": {"detect": {"width": 1280, "height": 720}},
                            "garage": {"friendly_name": "Garage", "detect": {"width": 1920, "height": 576}},
                            "off": {"enabled": false}}}"#,
        )
        .unwrap();
        let c = cameras_from_frigate(&v);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].id.as_str(), c[0].name.as_str(), c[0].live_grid.as_str(), c[0].live_full.as_str(), c[0].span), ("front", "Front", "front_sub", "front_main", 1));
        assert_eq!((c[1].name.as_str(), c[1].live_grid.as_str(), c[1].live_full.as_str(), c[1].span), ("Garage", "garage", "garage", 2));
    }
}
