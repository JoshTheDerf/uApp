//! Minimal client for REMOTE MCP servers over streamable HTTP.
//! Server config (stored in uapp_config key "mcp", an array):
//!   [{"name": "crm", "url": "https://…/mcp", "headers": {"Authorization": "Bearer …"}}]

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::Mutex;

static SESSIONS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn get_session(url: &str) -> Option<String> {
    SESSIONS.lock().unwrap().as_ref().and_then(|m| m.get(url).cloned())
}

fn set_session(url: &str, sid: Option<String>) {
    let mut g = SESSIONS.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    match sid {
        Some(s) => {
            m.insert(url.to_string(), s);
        }
        None => {
            m.remove(url);
        }
    }
}

fn post(server: &Value, body: &Value, session: Option<&str>) -> Result<(Value, Option<String>)> {
    let url = server["url"].as_str().ok_or_else(|| anyhow!("mcp server missing url"))?;
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".into(), "application/json".into()),
        ("Accept".into(), "application/json, text/event-stream".into()),
        ("MCP-Protocol-Version".into(), "2025-03-26".into()),
    ];
    if let Some(h) = server["headers"].as_object() {
        for (k, v) in h {
            if let Some(vs) = v.as_str() {
                headers.push((k.clone(), vs.to_string()));
            }
        }
    }
    if let Some(sid) = session {
        headers.push(("Mcp-Session-Id".into(), sid.to_string()));
    }
    let hdrs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let resp = crate::net::request("POST", url, &hdrs, Some(&body.to_string()), 20, 60)
        .map_err(|e| anyhow!("MCP request failed: {e}"))?;
    if resp.status >= 400 {
        let code = resp.status;
        let b = resp.into_string().unwrap_or_default();
        let snippet = b.chars().take(300).collect::<String>();
        // 401/404 with a session set = the server rejected our session
        // before running anything; safe to re-initialize and retry even a
        // non-idempotent call. Any other status (or a transport error) is
        // NOT safe to replay — the call may already have executed.
        if session.is_some() && (code == 401 || code == 404) {
            bail!("{SESSION_EXPIRED}: MCP server returned {code}: {snippet}");
        }
        bail!("MCP server returned {code}: {snippet}");
    }
    let new_session = resp.header("Mcp-Session-Id").map(|s| s.to_string());
    let ctype = resp.content_type.clone();
    if ctype.contains("event-stream") {
        // Take the last data: line carrying a JSON-RPC result/error.
        let reader = resp.reader;
        let mut last: Option<Value> = None;
        for line in reader.lines() {
            let line = line?;
            if let Some(data) = line.strip_prefix("data:") {
                if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                    if v.get("result").is_some() || v.get("error").is_some() {
                        last = Some(v);
                    }
                }
            }
        }
        Ok((last.unwrap_or(json!({})), new_session))
    } else {
        let text = resp.into_string()?;
        if text.trim().is_empty() {
            return Ok((json!({}), new_session)); // notifications get 202/empty
        }
        Ok((serde_json::from_str(&text)?, new_session))
    }
}

fn rpc(server: &Value, session: Option<&str>, method: &str, params: Value) -> Result<(Value, Option<String>)> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let (resp, sid) = post(server, &body, session)?;
    if let Some(err) = resp.get("error") {
        bail!("MCP {method} error: {}", err["message"].as_str().unwrap_or("unknown"));
    }
    Ok((resp["result"].clone(), sid))
}

fn ensure_session(server: &Value) -> Result<Option<String>> {
    let url = server["url"].as_str().unwrap_or_default();
    if let Some(s) = get_session(url) {
        return Ok(Some(s));
    }
    let (_, sid) = rpc(
        server,
        None,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "uapp", "version": env!("CARGO_PKG_VERSION")}
        }),
    )?;
    // Best-effort initialized notification.
    let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let _ = post(server, &note, sid.as_deref());
    set_session(url, sid.clone());
    Ok(sid)
}

/// Marker on errors that mean "the server rejected our session before doing
/// any work" — the only case where retrying a non-idempotent call is safe.
const SESSION_EXPIRED: &str = "__mcp_session_expired__";

/// Run `f` with a live session id. `idempotent` controls the retry policy:
/// - true (tools/list, initialize): any failure is safe to retry once.
/// - false (tools/call): retry ONLY on an explicit session rejection, since a
///   transport/timeout error could mean the tool already ran remotely.
fn with_session<T>(
    server: &Value,
    idempotent: bool,
    f: impl Fn(Option<&str>) -> Result<T>,
) -> Result<T> {
    let sid = ensure_session(server)?;
    match f(sid.as_deref()) {
        Ok(v) => Ok(v),
        Err(e) => {
            let session_expired = e.to_string().contains(SESSION_EXPIRED);
            if !idempotent && !session_expired {
                return Err(e); // ambiguous: don't risk double-executing
            }
            set_session(server["url"].as_str().unwrap_or_default(), None);
            let sid = ensure_session(server)?;
            f(sid.as_deref())
        }
    }
}

/// [{server, name, description, input_schema}] across all configured servers.
/// A server that fails to answer is skipped (logged) rather than fatal.
pub fn list_all_tools(servers: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for srv in servers {
        let name = srv["name"].as_str().unwrap_or("mcp").to_string();
        let listed = with_session(srv, true, |sid| {
            let (result, _) = rpc(srv, sid, "tools/list", json!({}))?;
            Ok(result)
        });
        match listed {
            Ok(result) => {
                for t in result["tools"].as_array().cloned().unwrap_or_default() {
                    out.push(json!({
                        "server": name,
                        "name": t["name"],
                        "description": t["description"].as_str().unwrap_or(""),
                        "input_schema": if t["inputSchema"].is_object() { t["inputSchema"].clone() }
                                        else { json!({"type": "object", "properties": {}}) },
                    }));
                }
            }
            Err(e) => eprintln!("uapp: MCP server '{name}' unavailable: {e}"),
        }
    }
    out
}

pub fn call_tool(server: &Value, tool: &str, args: Value) -> Result<Value> {
    with_session(server, false, |sid| {
        let (result, _) = rpc(
            server,
            sid,
            "tools/call",
            json!({"name": tool, "arguments": args}),
        )?;
        // Flatten text content for the model; keep structured content if present.
        let mut texts = Vec::new();
        for c in result["content"].as_array().cloned().unwrap_or_default() {
            if c["type"] == "text" {
                texts.push(c["text"].as_str().unwrap_or("").to_string());
            }
        }
        let mut out = json!({"text": texts.join("\n")});
        if result.get("structuredContent").is_some() {
            out["structured"] = result["structuredContent"].clone();
        }
        if result["isError"].as_bool().unwrap_or(false) {
            bail!("{}", texts.join("\n"));
        }
        Ok(out)
    })
}
