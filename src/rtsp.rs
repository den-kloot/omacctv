//! A minimal RTSP client (TCP, digest/basic auth): enough to ask a camera what it offers
//! (DESCRIBE, used by `omacctv add` / `scan`) and to open the ONVIF audio backchannel for
//! two-way audio straight from the phone (PCMU over interleaved RTP).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const BACKCHANNEL: &str = "www.onvif.org/ver20/backchannel";

pub struct Url {
    pub user: String,
    pub pass: String,
    pub host: String,
    pub port: u16,
    /// Path with query, e.g. `/h264Preview_01_main`.
    pub path: String,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
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

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let rest = url.strip_prefix("rtsp://").ok_or("only rtsp:// urls")?;
        let (auth, hostpath) = match rest.rfind('@') {
            Some(i) if !rest[..i].contains('/') => (Some(&rest[..i]), &rest[i + 1..]),
            _ => (None, rest),
        };
        let (user, pass) = match auth {
            Some(a) => match a.split_once(':') {
                Some((u, p)) => (unescape(u), unescape(p)),
                None => (unescape(a), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let (hp, path) = match hostpath.find('/') {
            Some(i) => (&hostpath[..i], &hostpath[i..]),
            None => (hostpath, "/"),
        };
        let (host, port) = match hp.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port in {hp}"))?),
            None => (hp, 554),
        };
        if host.is_empty() {
            return Err("no host".into());
        }
        Ok(Url { user, pass, host: host.into(), port, path: path.into() })
    }

    /// The URL without credentials (what goes into requests and logs).
    pub fn base(&self) -> String {
        format!("rtsp://{}:{}{}", self.host, self.port, self.path)
    }
}

/// `rtsp://user:****@host/...` for printing.
pub fn redact(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) if !u.pass.is_empty() => format!("rtsp://{}:****@{}:{}{}", u.user, u.host, u.port, u.path),
        _ => url.into(),
    }
}

pub struct Conn {
    url: Url,
    pub s: TcpStream,
    cseq: u32,
    digest: Option<(String, String)>,
    basic: bool,
    pub session: Option<String>,
    buf: Vec<u8>,
}

pub struct Reply {
    pub code: u16,
    pub head: String,
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn quoted(head: &str, key: &str) -> Option<String> {
    let i = head.find(&format!("{key}=\""))? + key.len() + 2;
    let j = head[i..].find('"')?;
    Some(head[i..i + j].to_string())
}

impl Conn {
    pub fn open(url: &str, timeout: Duration) -> Result<Conn, String> {
        let url = Url::parse(url)?;
        let addr = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| format!("{}: {e}", url.host))?
            .next()
            .ok_or("no address")?;
        let s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("{}:{}: {e}", url.host, url.port))?;
        s.set_read_timeout(Some(timeout)).ok();
        s.set_write_timeout(Some(timeout)).ok();
        s.set_nodelay(true).ok();
        Ok(Conn { url, s, cseq: 0, digest: None, basic: false, session: None, buf: Vec::new() })
    }

    pub fn base(&self) -> String {
        self.url.base()
    }

    fn auth(&self, method: &str, uri: &str) -> String {
        if let Some((realm, nonce)) = &self.digest {
            let ha1 = crate::md5::hex(format!("{}:{realm}:{}", self.url.user, self.url.pass).as_bytes());
            let ha2 = crate::md5::hex(format!("{method}:{uri}").as_bytes());
            let r = crate::md5::hex(format!("{ha1}:{nonce}:{ha2}").as_bytes());
            format!(
                "Authorization: Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{r}\"\r\n",
                self.url.user
            )
        } else if self.basic {
            format!("Authorization: Basic {}\r\n", crate::md5::base64(format!("{}:{}", self.url.user, self.url.pass).as_bytes()))
        } else {
            String::new()
        }
    }

    fn read_reply(&mut self) -> Result<Reply, String> {
        loop {
            // Skip interleaved RTP/RTCP packets the camera may send.
            while self.buf.first() == Some(&b'$') && self.buf.len() >= 4 {
                let n = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                if self.buf.len() < 4 + n {
                    break;
                }
                self.buf.drain(..4 + n);
            }
            if self.buf.first() != Some(&b'$') {
                if let Some(end) = find(&self.buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&self.buf[..end]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if self.buf.len() >= end + 4 + len {
                        let body = String::from_utf8_lossy(&self.buf[end + 4..end + 4 + len]).to_string();
                        self.buf.drain(..end + 4 + len);
                        let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
                        if !head.starts_with("RTSP/") {
                            return Err("not an RTSP server".into());
                        }
                        return Ok(Reply { code, head, body });
                    }
                }
            }
            let mut b = [0u8; 16384];
            let n = self.s.read(&mut b).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("connection closed".into());
            }
            self.buf.extend_from_slice(&b[..n]);
        }
    }

    /// One request; a 401 is answered once with the camera's auth challenge.
    pub fn req(&mut self, method: &str, uri: &str, extra: &str) -> Result<Reply, String> {
        for _ in 0..2 {
            self.cseq += 1;
            let sess = self.session.as_ref().map(|s| format!("Session: {s}\r\n")).unwrap_or_default();
            let msg = format!(
                "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: omacctv\r\nRequire: {BACKCHANNEL}\r\n{sess}{extra}{}\r\n",
                self.cseq,
                self.auth(method, uri)
            );
            self.s.write_all(msg.as_bytes()).map_err(|e| e.to_string())?;
            let r = self.read_reply()?;
            if r.code == 401 && self.digest.is_none() && !self.basic && !self.url.user.is_empty() {
                let h = r.header("WWW-Authenticate").unwrap_or("").to_string();
                let all = r.head.clone();
                if all.contains("Digest") {
                    let realm = quoted(&all, "realm").unwrap_or_default();
                    let nonce = quoted(&all, "nonce").unwrap_or_default();
                    self.digest = Some((realm, nonce));
                } else if h.contains("Basic") {
                    self.basic = true;
                } else {
                    return Ok(r);
                }
                continue;
            }
            return Ok(r);
        }
        Err(format!("{method}: login failed"))
    }

    pub fn send_interleaved(&mut self, channel: u8, packet: &[u8]) -> std::io::Result<()> {
        let mut b = Vec::with_capacity(packet.len() + 4);
        b.push(b'$');
        b.push(channel);
        b.extend_from_slice(&(packet.len() as u16).to_be_bytes());
        b.extend_from_slice(packet);
        self.s.write_all(&b)
    }

    pub fn keepalive(&mut self) -> std::io::Result<()> {
        self.cseq += 1;
        let base = self.base();
        let msg = format!(
            "GET_PARAMETER {base} RTSP/1.0\r\nCSeq: {}\r\nSession: {}\r\n{}\r\n",
            self.cseq,
            self.session.clone().unwrap_or_default(),
            self.auth("GET_PARAMETER", &base)
        );
        self.s.write_all(msg.as_bytes())
    }

    pub fn teardown(&mut self) {
        self.cseq += 1;
        let base = self.base();
        let msg = format!(
            "TEARDOWN {base} RTSP/1.0\r\nCSeq: {}\r\nSession: {}\r\n{}\r\n",
            self.cseq,
            self.session.clone().unwrap_or_default(),
            self.auth("TEARDOWN", &base)
        );
        let _ = self.s.write_all(msg.as_bytes());
    }
}

/// What a camera's DESCRIBE says.
#[derive(Debug, Default)]
pub struct Info {
    pub server: String,
    pub video: Option<String>,
    pub audio: Option<String>,
    /// The ONVIF backchannel track's control URL when it offers PCMU (G.711 mu-law).
    pub backchannel: Option<String>,
    /// Some backchannel exists, but not in PCMU.
    pub backchannel_other: Option<String>,
}

pub fn parse_sdp(sdp: &str, content_base: &str) -> Info {
    let mut info = Info::default();
    for sec in sdp.split("\nm=").skip(1) {
        let first = sec.lines().next().unwrap_or("");
        let kind = first.split_whitespace().next().unwrap_or("");
        let codec = sec
            .lines()
            .find_map(|l| l.trim().strip_prefix("a=rtpmap:"))
            .and_then(|r| r.split_whitespace().nth(1))
            .map(|c| c.split('/').next().unwrap_or(c).to_string())
            .unwrap_or_else(|| first.split_whitespace().nth(3).unwrap_or("?").to_string());
        let sendonly = sec.lines().any(|l| l.trim() == "a=sendonly");
        let control = sec.lines().find_map(|l| l.trim().strip_prefix("a=control:")).unwrap_or("");
        match kind {
            "video" if info.video.is_none() => info.video = Some(codec),
            "audio" if sendonly => {
                let pcmu = first.split_whitespace().skip(3).any(|pt| pt == "0") || codec.eq_ignore_ascii_case("PCMU");
                if pcmu {
                    let uri = if control.starts_with("rtsp://") {
                        control.to_string()
                    } else {
                        format!("{}/{}", content_base.trim_end_matches('/'), control)
                    };
                    info.backchannel = Some(uri);
                } else {
                    info.backchannel_other = Some(codec);
                }
            }
            "audio" if info.audio.is_none() => info.audio = Some(codec),
            _ => {}
        }
    }
    info
}

/// DESCRIBE with the ONVIF backchannel requirement.
pub fn describe(conn: &mut Conn) -> Result<Info, String> {
    let base = conn.base();
    let r = conn.req("DESCRIBE", &base, "Accept: application/sdp\r\n")?;
    match r.code {
        200 => {}
        401 => return Err("401 unauthorized (user/password needed in the URL)".into()),
        404 => return Err("404 stream not found (wrong path)".into()),
        c => return Err(format!("DESCRIBE answered {c}")),
    }
    let cb = r.header("Content-Base").map(String::from).unwrap_or_else(|| base.clone());
    let mut info = parse_sdp(&r.body, &cb);
    info.server = r.header("Server").unwrap_or("").to_string();
    if info.server.is_empty() {
        // go2rtc names itself only in the SDP session line ("s=go2rtc/1.9.14").
        info.server = r.body.lines().find_map(|l| l.trim().strip_prefix("s=")).unwrap_or("").to_string();
    }
    Ok(info)
}

pub fn probe(url: &str) -> Result<Info, String> {
    let mut c = Conn::open(url, Duration::from_secs(5))?;
    let info = describe(&mut c);
    c.teardown();
    info
}

/// An open backchannel: send 8 kHz mu-law with `send`.
pub struct Backchannel {
    conn: Conn,
    channel: u8,
    seq: u16,
    ts: u32,
    ssrc: u32,
    last_keepalive: std::time::Instant,
}

impl Backchannel {
    pub fn open(url: &str) -> Result<Backchannel, String> {
        let mut conn = Conn::open(url, Duration::from_secs(5))?;
        let info = describe(&mut conn)?;
        let track = info.backchannel.ok_or_else(|| match info.backchannel_other {
            Some(c) => format!("the camera's backchannel is {c}, only PCMU is supported"),
            None => "this camera has no speaker (no ONVIF backchannel)".to_string(),
        })?;
        let r = conn.req("SETUP", &track, "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n")?;
        if r.code != 200 {
            return Err(format!("SETUP answered {}", r.code));
        }
        conn.session = r.header("Session").map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
        let channel = r
            .header("Transport")
            .and_then(|t| t.split("interleaved=").nth(1))
            .and_then(|t| t.split(['-', ';']).next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let base = conn.base();
        let r = conn.req("PLAY", &base, "Range: npt=0.000-\r\n")?;
        if r.code != 200 {
            return Err(format!("PLAY answered {}", r.code));
        }
        // Whatever the camera sends from now on is read and thrown away.
        if let Ok(mut rd) = conn.s.try_clone() {
            rd.set_read_timeout(None).ok();
            std::thread::spawn(move || {
                let mut b = [0u8; 8192];
                while matches!(rd.read(&mut b), Ok(n) if n > 0) {}
            });
        }
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(7);
        Ok(Backchannel {
            conn,
            channel,
            seq: seed as u16,
            ts: seed.wrapping_mul(2654435761),
            ssrc: seed.rotate_left(13) ^ 0x5eed,
            last_keepalive: std::time::Instant::now(),
        })
    }

    /// `data`: mu-law samples; sent as 20 ms RTP packets by the caller's pacing.
    pub fn send(&mut self, data: &[u8]) -> std::io::Result<()> {
        for chunk in data.chunks(160) {
            let mut p = Vec::with_capacity(12 + chunk.len());
            p.extend_from_slice(&[0x80, 0]);
            p.extend_from_slice(&self.seq.to_be_bytes());
            p.extend_from_slice(&self.ts.to_be_bytes());
            p.extend_from_slice(&self.ssrc.to_be_bytes());
            p.extend_from_slice(chunk);
            self.conn.send_interleaved(self.channel, &p)?;
            self.seq = self.seq.wrapping_add(1);
            self.ts = self.ts.wrapping_add(chunk.len() as u32);
        }
        if self.last_keepalive.elapsed() > Duration::from_secs(20) {
            self.last_keepalive = std::time::Instant::now();
            self.conn.keepalive()?;
        }
        Ok(())
    }
}

impl Drop for Backchannel {
    fn drop(&mut self) {
        self.conn.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = Url::parse("rtsp://admin:p%40ss:x@10.0.0.5:8554/h264Preview_01_main").unwrap();
        assert_eq!((u.user.as_str(), u.pass.as_str(), u.host.as_str(), u.port), ("admin", "p@ss:x", "10.0.0.5", 8554));
        assert_eq!(u.base(), "rtsp://10.0.0.5:8554/h264Preview_01_main");
        let u = Url::parse("rtsp://cam.lan/stream1").unwrap();
        assert_eq!((u.user.as_str(), u.port, u.path.as_str()), ("", 554, "/stream1"));
        assert_eq!(redact("rtsp://a:secret@h/x"), "rtsp://a:****@h:554/x");
    }

    #[test]
    fn sdp_backchannel() {
        let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:track1\r\n\
                   m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/16000\r\na=recvonly\r\na=control:track2\r\n\
                   m=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\na=control:track3\r\n";
        let i = parse_sdp(sdp, "rtsp://h:554/x/");
        assert_eq!(i.video.as_deref(), Some("H264"));
        assert_eq!(i.audio.as_deref(), Some("MPEG4-GENERIC"));
        assert_eq!(i.backchannel.as_deref(), Some("rtsp://h:554/x/track3"));
        let i = parse_sdp("v=0\nm=video 0 RTP/AVP 96\na=rtpmap:96 H265/90000\n", "rtsp://h/");
        assert_eq!((i.video.as_deref(), i.backchannel.is_none()), (Some("H265"), true));
    }
}
