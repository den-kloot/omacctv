//! `omacctv scan`: find cameras and NVRs on the local network.
//!
//! - ONVIF WS-Discovery (SOAP probe to 239.255.255.250:3702): cameras answer with their
//!   service address and scopes (name, hardware).
//! - A TCP sweep of the /24 for RTSP (554), go2rtc (8554 RTSP, 1984 API) and Frigate (5000).
//! - RTSP hosts are asked (OPTIONS, DESCRIBE) for their server name and whether the stream
//!   needs a login; Frigate and go2rtc are asked for their cameras / streams.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const PORTS: &[(u16, &str)] = &[(554, "rtsp"), (8554, "go2rtc-rtsp"), (1984, "go2rtc-api"), (5000, "frigate")];

/// Stream paths of common brands, `{ch}` = channel 1.
const PATHS: &[(&str, &str, &str)] = &[
    ("reolink", "/h264Preview_01_main", "/h264Preview_01_sub"),
    ("hikvision", "/Streaming/Channels/101", "/Streaming/Channels/102"),
    ("dahua", "/cam/realmonitor?channel=1&subtype=0", "/cam/realmonitor?channel=1&subtype=1"),
    ("amcrest", "/cam/realmonitor?channel=1&subtype=0", "/cam/realmonitor?channel=1&subtype=1"),
    ("tapo", "/stream1", "/stream2"),
    ("tp-link", "/stream1", "/stream2"),
    ("axis", "/axis-media/media.amp", "/axis-media/media.amp?resolution=640x360"),
    ("ubiquiti", "/s0", "/s1"),
    ("foscam", "/videoMain", "/videoSub"),
];

#[derive(Default, Debug, Clone)]
struct Host {
    ports: Vec<u16>,
    onvif: Vec<String>,
    name: String,
    hardware: String,
    rtsp_server: String,
    rtsp_auth: Option<bool>,
    frigate: Option<String>,
    frigate_cams: Vec<String>,
    go2rtc_streams: Vec<String>,
}

/// This machine's address on the LAN (the route to a public address, nothing is sent).
fn local_ip() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) => Some(*a.ip()),
        _ => None,
    }
}

fn parse_net(arg: Option<&str>) -> Result<(Ipv4Addr, u8), String> {
    match arg {
        Some(a) => {
            let (ip, bits) = a.split_once('/').unwrap_or((a, "24"));
            let ip: Ipv4Addr = ip.parse().map_err(|_| format!("bad network {a} (e.g. 192.168.1.0/24)"))?;
            let bits: u8 = bits.parse().map_err(|_| format!("bad prefix in {a}"))?;
            if !(16..=32).contains(&bits) {
                return Err("prefix must be /16 to /32".into());
            }
            Ok((ip, bits))
        }
        None => local_ip().map(|ip| (ip, 24)).ok_or_else(|| "no network found; pass one, e.g. 192.168.1.0/24".into()),
    }
}

fn hosts_of(ip: Ipv4Addr, bits: u8) -> Vec<Ipv4Addr> {
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    let net = u32::from(ip) & mask;
    let size = 1u32 << (32 - bits);
    if size <= 2 {
        return (0..size).map(|i| Ipv4Addr::from(net + i)).collect();
    }
    (1..size - 1).map(|i| Ipv4Addr::from(net + i)).collect()
}

fn tag<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    // <d:XAddrs>…</d:XAddrs> with any namespace prefix.
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&format!(":{name}>")).or_else(|| rest.find(&format!("<{name}>"))) {
        let after = &rest[i..];
        let Some(open_end) = after.find('>') else { break };
        let body = &after[open_end + 1..];
        let Some(close) = body.find("</") else { break };
        out.push(body[..close].trim());
        rest = &body[close..];
        if let Some(j) = rest.find('>') {
            rest = &rest[j + 1..];
        }
    }
    out
}

fn unpercent(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into()
}

/// ONVIF WS-Discovery: address → (XAddrs, name, hardware).
/// The probe goes to the multicast group and to each address in `unicast` (some cameras
/// or Wi-Fi networks drop multicast).
fn ws_discovery(wait: Duration, unicast: &[Ipv4Addr]) -> BTreeMap<Ipv4Addr, (Vec<String>, String, String)> {
    let mut found = BTreeMap::new();
    let Ok(s) = UdpSocket::bind("0.0.0.0:0") else { return found };
    s.set_read_timeout(Some(Duration::from_millis(250))).ok();
    s.set_multicast_ttl_v4(2).ok();
    let n = crate::now() as u64;
    let id = format!("uuid:{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}", std::process::id(), n & 0xffff, n % 0xfff, (n >> 12) % 0xfff, n);
    let probe = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><e:Header><w:MessageID>{id}</w:MessageID><w:To e:mustUnderstand="true">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action e:mustUnderstand="true">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header><e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body></e:Envelope>"#
    );
    for _ in 0..2 {
        let _ = s.send_to(probe.as_bytes(), "239.255.255.250:3702");
    }
    for ip in unicast {
        let _ = s.send_to(probe.as_bytes(), SocketAddr::from((*ip, 3702)));
    }
    let t0 = Instant::now();
    let mut buf = vec![0u8; 65536];
    while t0.elapsed() < wait {
        let Ok((n, SocketAddr::V4(from))) = s.recv_from(&mut buf) else { continue };
        let xml = String::from_utf8_lossy(&buf[..n]);
        let xaddrs: Vec<String> =
            tag(&xml, "XAddrs").iter().flat_map(|x| x.split_whitespace().map(String::from)).collect();
        let scopes: Vec<String> =
            tag(&xml, "Scopes").iter().flat_map(|x| x.split_whitespace().map(String::from)).collect();
        let scope = |k: &str| {
            scopes
                .iter()
                .find_map(|s| s.strip_prefix(&format!("onvif://www.onvif.org/{k}/")).map(unpercent))
                .unwrap_or_default()
        };
        let e = found.entry(*from.ip()).or_insert((Vec::new(), String::new(), String::new()));
        for x in xaddrs {
            if !e.0.contains(&x) {
                e.0.push(x);
            }
        }
        if e.1.is_empty() {
            e.1 = scope("name");
        }
        if e.2.is_empty() {
            e.2 = scope("hardware");
        }
    }
    found
}

fn sweep(hosts: &[Ipv4Addr], timeout: Duration) -> BTreeMap<Ipv4Addr, Vec<u16>> {
    let jobs: Vec<(Ipv4Addr, u16)> = hosts.iter().flat_map(|h| PORTS.iter().map(move |p| (*h, p.0))).collect();
    let jobs = Arc::new(Mutex::new(jobs));
    let open = Arc::new(Mutex::new(BTreeMap::<Ipv4Addr, Vec<u16>>::new()));
    let threads: Vec<_> = (0..96)
        .map(|_| {
            let (jobs, open) = (jobs.clone(), open.clone());
            std::thread::spawn(move || loop {
                let Some((h, p)) = jobs.lock().unwrap().pop() else { break };
                if TcpStream::connect_timeout(&SocketAddr::from((h, p)), timeout).is_ok() {
                    open.lock().unwrap().entry(h).or_default().push(p);
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    let mut m = Arc::try_unwrap(open).map(|m| m.into_inner().unwrap()).unwrap_or_default();
    for v in m.values_mut() {
        v.sort();
    }
    m
}

fn inspect(ip: Ipv4Addr, h: &mut Host) {
    if h.ports.contains(&554) {
        if let Ok(mut c) = crate::rtsp::Conn::open(&format!("rtsp://{ip}:554/"), Duration::from_secs(2)) {
            if let Ok(r) = c.req("OPTIONS", &format!("rtsp://{ip}:554/"), "") {
                h.rtsp_server = r.header("Server").unwrap_or("").to_string();
            }
            if let Ok(r) = c.req("DESCRIBE", &format!("rtsp://{ip}:554/"), "Accept: application/sdp\r\n") {
                h.rtsp_auth = Some(r.code == 401);
                if h.rtsp_server.is_empty() {
                    h.rtsp_server = r.header("Server").unwrap_or("").to_string();
                }
            }
        }
    }
    if h.ports.contains(&5000) {
        if let Ok(v) = crate::frigate::get(&format!("http://{ip}:5000/api/version"), Duration::from_secs(3)) {
            let v = String::from_utf8_lossy(&v).trim().to_string();
            if v.chars().next().is_some_and(|c| c.is_ascii_digit()) && v.len() < 40 {
                h.frigate = Some(v);
                if let Ok(c) = crate::config::discover(&format!("http://{ip}:5000")) {
                    h.frigate_cams = c.into_iter().map(|c| c.id).collect();
                }
            }
        }
    }
    if h.ports.contains(&1984) {
        if let Ok(b) = crate::frigate::get(&format!("http://{ip}:1984/api/streams"), Duration::from_secs(3)) {
            if let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(&b) {
                h.go2rtc_streams = o.keys().cloned().collect();
            }
        }
    }
}

fn brand(h: &Host) -> Option<&'static (&'static str, &'static str, &'static str)> {
    let hay = format!("{} {} {}", h.hardware, h.name, h.rtsp_server).to_lowercase();
    PATHS.iter().find(|p| hay.contains(p.0)).or_else(|| {
        // Reolinks name their hardware e.g. "RLC-520A", "Duo 2V", "E1 Zoom".
        (hay.contains("rlc-") || hay.contains("duo") || hay.contains("e1 ")).then(|| &PATHS[0])
    })
}

pub fn run(args: &[String]) -> i32 {
    let as_json = args.iter().any(|a| a == "--json");
    let net = args.iter().find(|a| !a.starts_with("--")).map(String::as_str);
    let (ip, bits) = match parse_net(net) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("omacctv: {e}");
            return 2;
        }
    };
    let hosts = hosts_of(ip, bits);
    if !as_json {
        eprintln!("scanning {} hosts around {ip}/{bits} (ONVIF discovery + ports 554, 8554, 1984, 5000) …", hosts.len());
    }
    let open = sweep(&hosts, Duration::from_millis(500));
    let rtsp_hosts: Vec<Ipv4Addr> = open.iter().filter(|(_, p)| p.contains(&554)).map(|(ip, _)| *ip).collect();
    let onvif = ws_discovery(Duration::from_secs(2), &rtsp_hosts);

    let mut found: BTreeMap<Ipv4Addr, Host> = BTreeMap::new();
    for (ip, ports) in open {
        found.entry(ip).or_default().ports = ports;
    }
    for (ip, (x, name, hw)) in onvif {
        let h = found.entry(ip).or_default();
        h.onvif = x;
        h.name = name;
        h.hardware = hw;
    }
    let ips: Vec<Ipv4Addr> = found.keys().cloned().collect();
    let shared = Arc::new(Mutex::new(found));
    let threads: Vec<_> = ips
        .into_iter()
        .map(|ip| {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut h = shared.lock().unwrap()[&ip].clone();
                inspect(ip, &mut h);
                shared.lock().unwrap().insert(ip, h);
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    let found = Arc::try_unwrap(shared).map(|m| m.into_inner().unwrap()).unwrap_or_default();

    let mut out = Vec::new();
    for (ip, h) in &found {
        let mut hints = Vec::new();
        if let Some(v) = &h.frigate {
            hints.push(format!("omacctv discover http://{ip}:5000 --write"));
            let _ = v;
        }
        for s in h.go2rtc_streams.iter().take(12) {
            if h.frigate.is_none() {
                hints.push(format!("omacctv add rtsp://{ip}:8554/{s}"));
            }
        }
        if h.ports.contains(&554) || !h.onvif.is_empty() {
            let (main, sub) = brand(h).map(|b| (b.1, b.2)).unwrap_or(("/<stream path>", "/<sub stream path>"));
            let cred = if h.rtsp_auth != Some(false) { "USER:PASSWORD@" } else { "" };
            hints.push(format!("omacctv add rtsp://{cred}{ip}:554{sub} --full rtsp://{cred}{ip}:554{main}"));
        }
        let kind = if h.frigate.is_some() {
            "frigate"
        } else if !h.go2rtc_streams.is_empty() || h.ports.contains(&8554) {
            "go2rtc"
        } else if !h.onvif.is_empty() {
            "onvif camera"
        } else if h.ports.contains(&554) {
            "rtsp"
        } else {
            "other"
        };
        if kind == "other" {
            continue;
        }
        out.push(json!({
            "ip": ip.to_string(), "kind": kind, "ports": h.ports,
            "name": h.name, "hardware": h.hardware, "rtsp_server": h.rtsp_server,
            "rtsp_needs_login": h.rtsp_auth, "onvif": h.onvif,
            "frigate_version": h.frigate, "frigate_cameras": h.frigate_cams, "go2rtc_streams": h.go2rtc_streams,
            "next": hints,
        }));
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&Value::from(out)).unwrap_or_default());
        return 0;
    }
    if out.is_empty() {
        println!("nothing found (cameras on another VLAN? pass the network, e.g. omacctv scan 192.168.2.0/24)");
        return 1;
    }
    for h in &out {
        let s = |k: &str| h[k].as_str().unwrap_or("").to_string();
        let mut what = vec![s("kind")];
        for k in ["hardware", "name", "rtsp_server"] {
            let v = s(k);
            if !v.is_empty() && !what.contains(&v) {
                what.push(v);
            }
        }
        if let Some(v) = h["frigate_version"].as_str() {
            what.push(format!("Frigate {v}"));
        }
        let ports: Vec<String> = h["ports"].as_array().into_iter().flatten().map(|p| p.to_string()).collect();
        println!("{:<15} {}  [ports {}]", s("ip"), what.join(" · "), if ports.is_empty() { "-".into() } else { ports.join(",") });
        for k in ["frigate_cameras", "go2rtc_streams"] {
            let l: Vec<&str> = h[k].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
            if !l.is_empty() {
                println!("{:15} {}: {}", "", k.replace('_', " "), l.join(", "));
            }
        }
        if h["rtsp_needs_login"].as_bool() == Some(true) {
            println!("{:15} RTSP needs a login", "");
        }
        for n in h["next"].as_array().into_iter().flatten() {
            println!("{:15} → {}", "", n.as_str().unwrap_or(""));
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nets() {
        assert_eq!(hosts_of("192.168.1.77".parse().unwrap(), 24).len(), 254);
        assert_eq!(hosts_of("10.0.0.5".parse().unwrap(), 32), vec!["10.0.0.5".parse::<Ipv4Addr>().unwrap()]);
        assert!(parse_net(Some("10.0.0.0/8")).is_err());
    }

    #[test]
    fn probe_match() {
        let xml = r#"<s:Envelope><s:Body><d:ProbeMatches><d:ProbeMatch><d:Scopes>onvif://www.onvif.org/name/Front%20Door onvif://www.onvif.org/hardware/RLC-520A</d:Scopes><d:XAddrs>http://192.168.1.20:8000/onvif/device_service</d:XAddrs></d:ProbeMatch></d:ProbeMatches></s:Body></s:Envelope>"#;
        assert_eq!(tag(xml, "XAddrs"), vec!["http://192.168.1.20:8000/onvif/device_service"]);
        assert_eq!(unpercent("Front%20Door"), "Front Door");
        let h = Host { hardware: "RLC-520A".into(), ..Default::default() };
        assert_eq!(brand(&h).unwrap().1, "/h264Preview_01_main");
    }
}
