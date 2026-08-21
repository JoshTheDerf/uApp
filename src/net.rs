//! Portable HTTP for the AI providers, web tools and MCP client.
//!
//! Native: ureq (blocking, with a live streaming body so SSE deltas render as
//! they arrive). wasm32: a synchronous XMLHttpRequest performed by the worker
//! glue — the whole body arrives at once, so "streaming" responses are parsed
//! from the buffered text (still correct, just not incremental).
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
    Ok(Resp {
        status,
        content_type,
        headers,
        reader: Box::new(std::io::BufReader::new(resp.into_reader())),
    })
}

/// wasm: performed as a sync XHR by the worker glue (`js_http_request`), which
/// returns `{status, contentType, headers, bodyB64}` or `{error}` as JSON.
#[cfg(target_arch = "wasm32")]
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    _connect_timeout_secs: u64,
    _read_timeout_secs: u64,
) -> Result<Resp> {
    use base64::Engine as _;
    let hdrs: Vec<serde_json::Value> = headers
        .iter()
        .map(|(k, v)| serde_json::json!([k, v]))
        .collect();
    let reply = crate::wasm::http_request(
        method,
        url,
        &serde_json::Value::Array(hdrs).to_string(),
        body,
    );
    let v: serde_json::Value = serde_json::from_str(&reply)
        .map_err(|_| anyhow::anyhow!("bad reply from the HTTP glue"))?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!(
            "request failed: {e} (browser demo note: the target must allow cross-origin \
             requests — Anthropic, z.ai and OpenRouter do; most other hosts don't)"
        );
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(v["bodyB64"].as_str().unwrap_or(""))
        .unwrap_or_default();
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
    Ok(Resp {
        status: v["status"].as_u64().unwrap_or(0) as u16,
        content_type: v["contentType"].as_str().unwrap_or("").to_string(),
        headers,
        reader: Box::new(std::io::Cursor::new(bytes)),
    })
}
