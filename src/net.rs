//! Portable HTTP for the AI providers, web tools and MCP client.
//!
//! Native: ureq (blocking, with a live streaming body so SSE deltas render as
//! they arrive). wasm32: a `fetch` on the main thread whose body chunks reach
//! the worker through a SharedArrayBuffer — also live, so SSE deltas render as
//! they arrive there too. (Without cross-origin isolation the glue falls back
//! to a sync XHR and the whole body arrives at once.)
//!
//! Transport failures are `Err`; an HTTP error status is `Ok` with `status`
//! set, so each caller can keep its own error wording.

use anyhow::Result;
use std::io::BufRead;

pub struct Resp {
    pub status: u16,
    pub content_type: String,
    /// Response headers, lowercased names.
    pub headers: Vec<(String, String)>,
    pub reader: Box<dyn BufRead>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn into_string(mut self) -> Result<String> {
        let mut s = String::new();
        use std::io::Read;
        self.reader.read_to_string(&mut s)?;
        Ok(s)
    }
    /// Read at most `cap` bytes of the body.
    pub fn into_bytes_capped(mut self, cap: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        use std::io::Read;
        (&mut self.reader).take(cap).read_to_end(&mut out)?;
        Ok(out)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    connect_timeout_secs: u64,
    read_timeout_secs: u64,
) -> Result<Resp> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout_read(std::time::Duration::from_secs(read_timeout_secs))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) uapp/0.1")
        .build();
    let mut req = agent.request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let result = match body {
        Some(b) => req.send_string(b),
        None => req.call(),
    };
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_code, r)) => r,
        Err(e) => anyhow::bail!("request failed: {e}"),
    };
    Ok(into_resp(resp))
}

/// A GET of a URL that an app or the model chose (`web.fetch`,
/// `files.fetch`), which must not reach this machine or its network. Checking
/// the host before the request isn't enough on its own: DNS can answer
/// differently when the connection is made, and a public page can redirect
/// to `http://127.0.0.1/`. So every address is checked as the connection is
/// made (a resolver that refuses local ones), and redirects are followed here,
/// one hop at a time, each hop under the same rule.
#[cfg(not(target_arch = "wasm32"))]
pub fn get_public(url: &str, connect_timeout_secs: u64, read_timeout_secs: u64) -> Result<Resp> {
    const MAX_REDIRECTS: usize = 10;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout_read(std::time::Duration::from_secs(read_timeout_secs))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) uapp/0.1")
        .redirects(0)
        .resolver(PublicOnly)
        .build();
    let mut current = url::Url::parse(url).map_err(|e| anyhow::anyhow!("bad URL {url}: {e}"))?;
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(current.scheme(), "http" | "https") {
            anyhow::bail!("refusing to fetch a {}: URL", current.scheme());
        }
        let resp = match agent.get(current.as_str()).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_code, r)) => r,
            Err(e) => anyhow::bail!("request failed: {e}"),
        };
        if (300..400).contains(&resp.status()) {
            if let Some(loc) = resp.header("location") {
                current = current
                    .join(loc)
                    .map_err(|e| anyhow::anyhow!("bad redirect to {loc}: {e}"))?;
                continue;
            }
        }
        return Ok(into_resp(resp));
    }
    anyhow::bail!("too many redirects")
}

/// Resolves like the default, then refuses the lot if any address is
/// loopback, private, link-local or otherwise not on the public internet.
#[cfg(not(target_arch = "wasm32"))]
struct PublicOnly;

#[cfg(not(target_arch = "wasm32"))]
impl ureq::Resolver for PublicOnly {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = netloc.to_socket_addrs()?.collect();
        if let Some(a) = addrs.iter().find(|a| is_local_ip(a.ip())) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to connect to {netloc}: it resolves to a local/private address ({})",
                    a.ip()
                ),
            ));
        }
        Ok(addrs)
    }
}

/// Not somewhere on the public internet: loopback, private, link-local
/// (cloud metadata lives there), CGNAT, multicast, reserved, unspecified, and
/// IPv6 forms that carry one of those IPv4 addresses inside.
pub fn is_local_ip(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0/24
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18/15
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let embedded = |a: u16, b: u16| {
                is_local_ip(IpAddr::V4(Ipv4Addr::new(
                    (a >> 8) as u8,
                    a as u8,
                    (b >> 8) as u8,
                    b as u8,
                )))
            };
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // ULA
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || v6.to_ipv4_mapped().is_some_and(|v4| is_local_ip(IpAddr::V4(v4)))
                || (s[0..6] == [0, 0, 0, 0, 0, 0] && embedded(s[6], s[7])) // ::a.b.c.d
                || (s[0] == 0x64 && s[1] == 0xff9b && embedded(s[6], s[7])) // NAT64
                || (s[0] == 0x2002 && embedded(s[1], s[2])) // 6to4
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn into_resp(resp: ureq::Response) -> Resp {
    let status = resp.status();
    let content_type = resp.content_type().to_string();
    let headers = resp
        .headers_names()
        .into_iter()
        .filter_map(|n| {
            resp.header(&n)
                .map(|v| (n.to_ascii_lowercase(), v.to_string()))
        })
        .collect();
    Resp {
        status,
        content_type,
        headers,
        reader: Box::new(std::io::BufReader::new(resp.into_reader())),
    }
}

/// wasm: body of an in-flight request, pulled chunk by chunk from the glue.
#[cfg(target_arch = "wasm32")]
struct WasmBody {
    id: u32,
    read_timeout_ms: u64,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
}

#[cfg(target_arch = "wasm32")]
impl std::io::Read for WasmBody {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            if self.eof {
                return Ok(0);
            }
            let chunk = crate::wasm::http_read(self.id, self.read_timeout_ms)?;
            if chunk.is_empty() {
                self.eof = true;
                return Ok(0);
            }
            self.buf = chunk;
            self.pos = 0;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for WasmBody {
    fn drop(&mut self) {
        crate::wasm::http_close(self.id);
    }
}

/// wasm: the glue opens the request (`js_http_open`) and answers with the
/// headers `{id, status, contentType, headers}` (plus `bodyB64` when it had to
/// fall back to a sync XHR) or `{error}`; the body then streams via
/// `js_http_read`.
#[cfg(target_arch = "wasm32")]
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    connect_timeout_secs: u64,
    read_timeout_secs: u64,
) -> Result<Resp> {
    use base64::Engine as _;
    let hdrs: Vec<serde_json::Value> = headers
        .iter()
        .map(|(k, v)| serde_json::json!([k, v]))
        .collect();
    // A browser fetch() only settles once the response HEADERS arrive, so the
    // "connect" phase here spans the provider's whole time-to-first-byte — a
    // large prompt can take well over 20s before the first byte. Natively the
    // connect timeout covers the TCP handshake only and the read timeout the
    // wait for the response; the read timeout is the equivalent budget here.
    let reply = crate::wasm::http_open(
        method,
        url,
        &serde_json::Value::Array(hdrs).to_string(),
        body,
        connect_timeout_secs.max(read_timeout_secs) * 1000,
    );
    let v: serde_json::Value = serde_json::from_str(&reply)
        .map_err(|_| anyhow::anyhow!("bad reply from the HTTP glue"))?;
    if let Some(e) = v["error"].as_str() {
        // The CORS hint only helps when the browser refused/failed the fetch;
        // a timeout is the provider being slow, and saying "CORS" misleads.
        if e.contains("CORS") || e.contains("network") {
            anyhow::bail!(
                "request failed: {e} (browser note: the target must allow cross-origin \
                 requests — Anthropic, z.ai and OpenRouter do; most other hosts don't)"
            );
        }
        anyhow::bail!("request failed: {e}");
    }
    let headers = v["headers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|h| {
            Some((
                h[0].as_str()?.to_ascii_lowercase(),
                h[1].as_str()?.to_string(),
            ))
        })
        .collect();
    let reader: Box<dyn BufRead> = if let Some(b64) = v["bodyB64"].as_str() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap_or_default();
        Box::new(std::io::Cursor::new(bytes))
    } else {
        Box::new(std::io::BufReader::new(WasmBody {
            id: v["id"].as_u64().unwrap_or(0) as u32,
            read_timeout_ms: read_timeout_secs * 1000,
            buf: Vec::new(),
            pos: 0,
            eof: false,
        }))
    };
    Ok(Resp {
        status: v["status"].as_u64().unwrap_or(0) as u16,
        content_type: v["contentType"].as_str().unwrap_or("").to_string(),
        headers,
        reader,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_are_recognised() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "ff02::1",
        ] {
            assert!(is_local_ip(a.parse().unwrap()), "{a}");
        }
        for a in [
            "93.184.215.14",
            "1.1.1.1",
            "2606:4700::6810:84e5",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_local_ip(a.parse().unwrap()), "{a}");
        }
    }

    #[test]
    fn a_local_target_is_refused_at_connect_time() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
        let err = get_public(&url, 2, 2)
            .err()
            .expect("must refuse")
            .to_string();
        assert!(err.contains("local/private"), "{err}");
        // The same through a name that resolves to loopback.
        let url = format!("http://localhost:{}/", l.local_addr().unwrap().port());
        assert!(get_public(&url, 2, 2).is_err());
    }
}
