//! Minimal client for REMOTE MCP servers over streamable HTTP.
//! Server config (stored in uapp_config key "mcp", an array):
//!   [{"name": "crm", "url": "https://…/mcp", "headers": {"Authorization": "Bearer …"}}]
//!
//! Tool lists are cached per server (see [`list_all_tools_cached`]): the
//! assistant needs them for every run, and a server that is down must not
//! stall every chat message while its connection times out.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// The protocol version we ask for. The server answers with the version it
/// speaks, and that one is sent on every later request of the session.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// A live session with one server: its id (servers may not issue one) and
/// the negotiated protocol version.
#[derive(Clone)]
struct Session {
    id: Option<String>,
    version: String,
}

static SESSIONS: Mutex<Option<HashMap<String, Session>>> = Mutex::new(None);

/// JSON-RPC ids, unique for the life of the process: replies are matched to
/// requests by id, and a stream can carry other messages too.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn get_session(url: &str) -> Option<Session> {
    crate::app::lock(&SESSIONS)
        .as_ref()
        .and_then(|m| m.get(url).cloned())
}

fn set_session(url: &str, s: Option<Session>) {
    let mut g = crate::app::lock(&SESSIONS);
    let m = g.get_or_insert_with(HashMap::new);
    match s {
        Some(s) => {
            m.insert(url.to_string(), s);
        }
        None => {
            m.remove(url);
        }
    }
}

/// Connect/read timeouts in seconds: listing should be quick (a dead server
/// must not hold up a chat), a tool call may legitimately take a while.
#[derive(Clone, Copy)]
struct Timeouts(u64, u64);
const LIST: Timeouts = Timeouts(10, 20);
const CALL: Timeouts = Timeouts(20, 120);

fn headers_for(server: &Value, session: Option<&Session>) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "Accept".into(),
            "application/json, text/event-stream".into(),
        ),
        (
            "MCP-Protocol-Version".into(),
            session
                .map(|s| s.version.clone())
                .unwrap_or_else(|| PROTOCOL_VERSION.into()),
        ),
    ];
    if let Some(h) = server["headers"].as_object() {
        for (k, v) in h {
            if let Some(vs) = v.as_str() {
                headers.push((k.clone(), vs.to_string()));
            }
        }
    }
    if let Some(sid) = session.and_then(|s| s.id.as_deref()) {
        headers.push(("Mcp-Session-Id".into(), sid.to_string()));
    }
    headers
}

/// Fire-and-forget POST (notifications, our answers to server requests).
fn post_quiet(server: &Value, body: &Value, session: Option<&Session>) {
    let Some(url) = server["url"].as_str() else {
        return;
    };
    let headers = headers_for(server, session);
    let hdrs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let _ = crate::net::request("POST", url, &hdrs, Some(&body.to_string()), 10, 10);
}

/// What we say to a request the server sends us. We offer no client
/// capabilities, so anything but ping is "not supported".
pub(crate) fn answer_server_request(msg: &Value) -> Value {
    if msg["method"] == "ping" {
        json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}})
    } else {
        json!({"jsonrpc": "2.0", "id": msg["id"], "error": {
            "code": -32601,
            "message": format!("{} is not supported by this client", msg["method"].as_str().unwrap_or("?")),
        }})
    }
}

/// Pick our reply out of a sequence of JSON-RPC messages, answering any
/// requests the server makes along the way.
pub(crate) fn find_reply(
    messages: impl IntoIterator<Item = Value>,
    id: u64,
    mut answer: impl FnMut(Value),
) -> Option<Value> {
    for m in messages {
        // A batch is an array of messages.
        let batch = match m {
            Value::Array(a) => a,
            other => vec![other],
        };
        for m in batch {
            if m.get("method").is_some() {
                if m.get("id").is_some_and(|i| !i.is_null()) {
                    answer(answer_server_request(&m));
                }
                continue; // notifications: progress, logging, list changes
            }
            if m["id"].as_u64() == Some(id)
                && (m.get("result").is_some() || m.get("error").is_some())
            {
                return Some(m);
            }
        }
    }
    None
}

fn post(
    server: &Value,
    method: &str,
    params: Value,
    session: Option<&Session>,
    t: Timeouts,
) -> Result<(Value, Option<String>, Option<String>)> {
    let url = server["url"]
        .as_str()
        .ok_or_else(|| anyhow!("mcp server missing url"))?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let headers = headers_for(server, session);
    let hdrs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let resp = crate::net::request("POST", url, &hdrs, Some(&body.to_string()), t.0, t.1)
        .map_err(|e| anyhow!("MCP request failed: {e}"))?;
    if resp.status >= 400 {
        let code = resp.status;
        let b = resp.into_string().unwrap_or_default();
        let snippet = b.chars().take(300).collect::<String>();
        // 401/404 with a session set = the server rejected our session
        // before running anything; safe to re-initialize and retry even a
        // non-idempotent call. Any other status (or a transport error) is
        // NOT safe to replay — the call may already have executed.
        if session.is_some_and(|s| s.id.is_some()) && (code == 401 || code == 404) {
            bail!("{SESSION_EXPIRED}: MCP server returned {code}: {snippet}");
        }
        bail!("MCP server returned {code}: {snippet}");
    }
    let new_sid = resp.header("Mcp-Session-Id").map(|s| s.to_string());
    let answer = |reply: Value| post_quiet(server, &reply, session);
    let reply = if resp.content_type.contains("event-stream") {
        // Read events until our reply arrives; the server may send requests
        // and notifications first.
        let lines = resp
            .reader
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|line| {
                line.strip_prefix("data:")
                    .and_then(|d| serde_json::from_str::<Value>(d.trim()).ok())
            });
        find_reply(lines, id, answer)
    } else {
        let text = resp.into_string()?;
        if text.trim().is_empty() {
            None
        } else {
            find_reply([serde_json::from_str::<Value>(&text)?], id, answer)
        }
    };
    let reply = reply.ok_or_else(|| anyhow!("MCP {method}: the server sent no reply"))?;
    if let Some(err) = reply.get("error") {
        bail!(
            "MCP {method} error: {}",
            err["message"].as_str().unwrap_or("unknown")
        );
    }
    let version = reply["result"]["protocolVersion"]
        .as_str()
        .map(String::from);
    Ok((reply["result"].clone(), new_sid, version))
}

fn ensure_session(server: &Value) -> Result<Session> {
    let url = server["url"].as_str().unwrap_or_default();
    if let Some(s) = get_session(url) {
        return Ok(s);
    }
    let (_, sid, version) = post(
        server,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "uapp", "version": env!("CARGO_PKG_VERSION")}
        }),
        None,
        LIST,
    )?;
    let s = Session {
        id: sid,
        version: version.unwrap_or_else(|| PROTOCOL_VERSION.into()),
    };
    post_quiet(
        server,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        Some(&s),
    );
    set_session(url, Some(s.clone()));
    Ok(s)
}

/// Marker on errors that mean "the server rejected our session before doing
/// any work" — the only case where retrying a non-idempotent call is safe.
const SESSION_EXPIRED: &str = "__mcp_session_expired__";

/// Run `f` with a live session. `idempotent` controls the retry policy:
/// - true (tools/list, initialize): any failure is safe to retry once.
/// - false (tools/call): retry ONLY on an explicit session rejection, since a
///   transport/timeout error could mean the tool already ran remotely.
fn with_session<T>(
    server: &Value,
    idempotent: bool,
    f: impl Fn(&Session) -> Result<T>,
) -> Result<T> {
    let s = ensure_session(server)?;
    match f(&s) {
        Ok(v) => Ok(v),
        Err(e) => {
            let session_expired = e.to_string().contains(SESSION_EXPIRED);
            if !idempotent && !session_expired {
                return Err(e); // ambiguous: don't risk double-executing
            }
            set_session(server["url"].as_str().unwrap_or_default(), None);
            let s = ensure_session(server)?;
            f(&s)
        }
    }
}

/// One server's tools, fetched now: [{server, name, description, input_schema}].
fn fetch_tools(srv: &Value) -> Result<Vec<Value>> {
    let name = srv["name"].as_str().unwrap_or("mcp").to_string();
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    // Paged: follow nextCursor, within reason.
    for _ in 0..20 {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let result = with_session(srv, true, |s| {
            Ok(post(srv, "tools/list", params.clone(), Some(s), LIST)?.0)
        })?;
        for t in result["tools"].as_array().cloned().unwrap_or_default() {
            out.push(json!({
                "server": name,
                "name": t["name"],
                "description": t["description"].as_str().unwrap_or(""),
                "input_schema": if t["inputSchema"].is_object() { t["inputSchema"].clone() }
                                else { json!({"type": "object", "properties": {}}) },
            }));
        }
        cursor = result["nextCursor"].as_str().map(String::from);
        if cursor.is_none() {
            break;
        }
    }
    Ok(out)
}

/// [{server, name, description, input_schema}] across all configured servers,
/// fetched now. A server that fails to answer is skipped (logged) rather
/// than fatal. Also refreshes the cache.
pub fn list_all_tools(servers: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for srv in servers {
        out.extend(refresh(srv));
    }
    out
}

/// A cached tool list.
struct Cached {
    tools: Vec<Value>,
    fetched: u64,
    ok: bool,
    refreshing: bool,
}

static CACHE: Mutex<Option<HashMap<String, Cached>>> = Mutex::new(None);

/// Fresh lists are reused for this long; after that they are still served
/// while a refresh runs in the background.
const FRESH_MS: u64 = 5 * 60 * 1000;
/// A server that failed is not asked again for this long.
const RETRY_FAILED_MS: u64 = 60 * 1000;

fn cache_key(srv: &Value) -> String {
    json!([srv["url"], srv["headers"]]).to_string()
}

/// Fetch one server's tools now and remember the outcome.
fn refresh(srv: &Value) -> Vec<Value> {
    let (tools, ok) = match fetch_tools(srv) {
        Ok(t) => (t, true),
        Err(e) => {
            eprintln!(
                "uapp: MCP server '{}' unavailable: {e}",
                srv["name"].as_str().unwrap_or("mcp")
            );
            (Vec::new(), false)
        }
    };
    let mut g = crate::app::lock(&CACHE);
    g.get_or_insert_with(HashMap::new).insert(
        cache_key(srv),
        Cached {
            tools: tools.clone(),
            fetched: crate::store::now_ms(),
            ok,
            refreshing: false,
        },
    );
    tools
}

/// [`list_all_tools`] from the cache. A server seen in the last five
/// minutes is answered from memory; an older entry is still answered from
/// memory while a background refresh runs (in the browser build, where
/// there are no threads, it refreshes inline); a server that failed
/// recently is skipped for a minute instead of timing out again.
pub fn list_all_tools_cached(servers: &[Value]) -> Vec<Value> {
    let now = crate::store::now_ms();
    let mut out = Vec::new();
    for srv in servers {
        let key = cache_key(srv);
        let hit = {
            let mut g = crate::app::lock(&CACHE);
            match g.get_or_insert_with(HashMap::new).get_mut(&key) {
                None => None,
                Some(c) => {
                    let age = now.saturating_sub(c.fetched);
                    let stale = if c.ok {
                        age > FRESH_MS
                    } else {
                        age > RETRY_FAILED_MS
                    };
                    let refresh_now = stale && !c.refreshing;
                    if refresh_now {
                        c.refreshing = true;
                    }
                    Some((c.tools.clone(), refresh_now))
                }
            }
        };
        match hit {
            None => out.extend(refresh(srv)),
            Some((tools, refresh_now)) => {
                if refresh_now {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let srv = srv.clone();
                        std::thread::spawn(move || {
                            refresh(&srv);
                        });
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        out.extend(refresh(srv));
                        continue;
                    }
                }
                out.extend(tools);
            }
        }
    }
    out
}

/// One content item of a tool result as text. Images, audio and binary
/// resources can't ride along in a text result, so they become a note
/// saying what was there.
pub(crate) fn content_text(c: &Value) -> String {
    let size = |data: &Value| data.as_str().map_or(0, |d| d.len() * 3 / 4).div_ceil(1024);
    match c["type"].as_str().unwrap_or("") {
        "text" => c["text"].as_str().unwrap_or("").to_string(),
        kind @ ("image" | "audio") => format!(
            "[{kind} result ({}, {}KB), not shown]",
            c["mimeType"].as_str().unwrap_or("unknown type"),
            size(&c["data"])
        ),
        "resource" => {
            let r = &c["resource"];
            match r["text"].as_str() {
                Some(t) => format!("[resource {}]\n{t}", r["uri"].as_str().unwrap_or("")),
                None => format!(
                    "[resource {} ({}, {}KB), not shown]",
                    r["uri"].as_str().unwrap_or(""),
                    r["mimeType"].as_str().unwrap_or("binary"),
                    size(&r["blob"])
                ),
            }
        }
        "resource_link" => format!(
            "[resource link: {} {}]",
            c["name"].as_str().unwrap_or(""),
            c["uri"].as_str().unwrap_or("")
        ),
        other => format!("[{other} result, not shown]"),
    }
}

pub fn call_tool(server: &Value, tool: &str, args: Value) -> Result<Value> {
    with_session(server, false, |s| {
        let (result, _, _) = post(
            server,
            "tools/call",
            json!({"name": tool, "arguments": args}),
            Some(s),
            CALL,
        )?;
        // Flatten content for the model; keep structured content if present.
        let texts: Vec<String> = result["content"]
            .as_array()
            .map(|a| a.iter().map(content_text).collect())
            .unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_is_matched_by_id_and_server_requests_are_answered() {
        let mut answered = Vec::new();
        let msgs = vec![
            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 99, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": "s1", "method": "sampling/createMessage", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 6, "result": {"wrong": true}}),
            json!([{"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}]),
        ];
        let r = find_reply(msgs, 7, |a| answered.push(a)).unwrap();
        assert_eq!(r["result"]["ok"], true);
        assert_eq!(answered.len(), 2);
        assert_eq!(
            answered[0],
            json!({"jsonrpc": "2.0", "id": 99, "result": {}})
        );
        assert_eq!(answered[1]["error"]["code"], -32601);
        assert!(find_reply(vec![json!({"id": 1, "result": {}})], 2, |_| {}).is_none());
    }

    #[test]
    fn non_text_content_becomes_a_note() {
        assert_eq!(content_text(&json!({"type": "text", "text": "hi"})), "hi");
        let img = content_text(
            &json!({"type": "image", "mimeType": "image/png", "data": "A".repeat(4096)}),
        );
        assert_eq!(img, "[image result (image/png, 3KB), not shown]");
        let res = content_text(
            &json!({"type": "resource", "resource": {"uri": "file:///a.txt", "text": "body"}}),
        );
        assert!(res.contains("file:///a.txt") && res.ends_with("body"));
        let blob =
            content_text(&json!({"type": "resource", "resource": {"uri": "x", "blob": "AAAA"}}));
        assert!(blob.contains("not shown"));
        assert!(
            content_text(&json!({"type": "resource_link", "uri": "u", "name": "n"}))
                .contains("n u")
        );
    }

    #[test]
    fn ids_are_unique() {
        let a = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let b = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(a, b);
    }
}
