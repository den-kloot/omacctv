//! The little of Frigate's HTTP API this app needs: recordings (which times have footage),
//! review items (alerts / detections for the timeline) and recording snapshots for scrubbing.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;

/// Plain HTTP/1.0 GET (Frigate is on the LAN, no TLS; 1.0 = no chunked bodies).
pub fn get(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// urls")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:80") };
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| format!("{addr}: {e}"))?
        .next()
        .ok_or("no address")?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(4)).map_err(|e| format!("{addr}: {e}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    let host = hostport.split(':').next().unwrap_or(hostport);
    write!(s, "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: omacctv\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").ok_or("bad http reply")?;
    let head = String::from_utf8_lossy(&buf[..end]);
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        return Err(format!("HTTP {status} for {path}"));
    }
    Ok(buf[end + 4..].to_vec())
}

fn get_json(url: &str) -> Result<Value, String> {
    let body = get(url, Duration::from_secs(20))?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

#[derive(Clone, Debug)]
pub struct Event {
    pub cam: usize,
    pub start: f64,
    pub end: f64,
    pub alert: bool,
    #[allow(dead_code)] // for an event list later
    pub objects: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Data {
    /// Per camera: merged, sorted spans of recorded footage.
    pub runs: Vec<Vec<(f64, f64)>>,
    pub events: Vec<Event>,
    pub loaded: bool,
}

impl Data {
    /// The recorded span of camera `cam` that contains `t`.
    pub fn run_at(&self, cam: usize, t: f64) -> Option<(f64, f64)> {
        let runs = self.runs.get(cam)?;
        let i = runs.partition_point(|r| r.1 <= t + 0.05);
        let r = *runs.get(i)?;
        (r.0 <= t + 0.6).then_some(r)
    }

    /// The last recorded span of camera `cam` that ended before `t`.
    pub fn last_before(&self, cam: usize, t: f64) -> Option<(f64, f64)> {
        let runs = self.runs.get(cam)?;
        let i = runs.partition_point(|r| r.1 <= t + 0.05);
        i.checked_sub(1).map(|k| runs[k])
    }

    /// Earliest footage start after `t` among `cams`.
    pub fn next_start(&self, cams: &[usize], t: f64) -> Option<f64> {
        cams.iter()
            .filter_map(|&c| {
                let runs = self.runs.get(c)?;
                let i = runs.partition_point(|r| r.0 <= t + 0.6);
                runs.get(i).map(|r| r.0)
            })
            .min_by(|a, b| a.total_cmp(b))
    }

    /// Union of the runs of `cams` (for drawing).
    pub fn union(&self, cams: &[usize]) -> Vec<(f64, f64)> {
        let mut all: Vec<(f64, f64)> =
            cams.iter().filter_map(|&c| self.runs.get(c)).flatten().copied().collect();
        merge(&mut all, 2.0)
    }
}

fn merge(spans: &mut Vec<(f64, f64)>, gap: f64) -> Vec<(f64, f64)> {
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for &(s, e) in spans.iter() {
        match out.last_mut() {
            Some(last) if s <= last.1 + gap => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// Recordings and review items for the last `days` days.
pub fn load(cfg: &Config, days: f64) -> Result<Data, String> {
    let now = crate::now();
    let after = now - days * 86400.0;
    let mut runs = Vec::new();
    let ids: Vec<&str> = cfg.cameras.iter().map(|c| c.id.as_str()).collect();
    let recorded: Vec<&str> = cfg.cameras.iter().filter(|c| c.recorded()).map(|c| c.id.as_str()).collect();
    if !cfg.has_frigate || recorded.is_empty() {
        return Ok(Data { runs: vec![Vec::new(); ids.len()], events: Vec::new(), loaded: true });
    }
    for c in &cfg.cameras {
        if !c.recorded() {
            runs.push(Vec::new());
            continue;
        }
        let v = get_json(&format!("{}/api/{}/recordings?after={after:.0}&before={:.0}", cfg.frigate, c.id, now + 60.0))?;
        let mut spans: Vec<(f64, f64)> = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| Some((r["start_time"].as_f64()?, r["end_time"].as_f64()?)))
                    .collect()
            })
            .unwrap_or_default();
        // Segments are ~10 s with tiny seams between them.
        runs.push(merge(&mut spans, 1.5));
    }
    let v = get_json(&format!(
        "{}/api/review?cameras={}&after={after:.0}&before={:.0}&limit=5000",
        cfg.frigate,
        recorded.join(","),
        now + 60.0
    ))?;
    let mut events: Vec<Event> = v
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let cam = ids.iter().position(|id| Some(*id) == r["camera"].as_str())?;
                    let start = r["start_time"].as_f64()?;
                    Some(Event {
                        cam,
                        start,
                        end: r["end_time"].as_f64().unwrap_or(now),
                        alert: r["severity"].as_str() == Some("alert"),
                        objects: r["data"]["objects"]
                            .as_array()
                            .map(|o| o.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    events.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(Data { runs, events, loaded: true })
}

/// A JPEG of camera `id` at time `t`, `height` pixels high.
pub fn snapshot(cfg: &Config, id: &str, t: f64, height: i32) -> Result<Vec<u8>, String> {
    get(
        &format!("{}/api/{id}/recordings/{t:.2}/snapshot.jpg?height={height}", cfg.frigate),
        Duration::from_secs(8),
    )
}

/// HLS playlist of camera `id` from `start` to `end` (Frigate's nginx VOD).
pub fn vod_url(cfg: &Config, id: &str, start: f64, end: f64) -> String {
    format!("{}/vod/{id}/start/{start:.2}/end/{end:.2}/master.m3u8", cfg.frigate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging_and_lookup() {
        let mut v = vec![(20.0, 30.0), (0.0, 10.0), (10.5, 19.8), (100.0, 110.0)];
        let runs = merge(&mut v, 1.5);
        assert_eq!(runs, vec![(0.0, 30.0), (100.0, 110.0)]);
        let d = Data { runs: vec![runs], events: vec![], loaded: true };
        assert_eq!(d.run_at(0, 5.0), Some((0.0, 30.0)));
        assert_eq!(d.run_at(0, 50.0), None);
        assert_eq!(d.run_at(0, 99.7), Some((100.0, 110.0)));
        assert_eq!(d.next_start(&[0], 50.0), Some(100.0));
        assert_eq!(d.next_start(&[0], 105.0), None);
        assert_eq!(d.last_before(0, 50.0), Some((0.0, 30.0)));
        assert_eq!(d.last_before(0, 200.0), Some((100.0, 110.0)));
        assert_eq!(d.last_before(0, -5.0), None);
    }
}
