//! Built-in tool registry for the AI assistant.
//!
//! Adding a new tool = one `ToolDef` entry in `registry()`. Set `gated: true`
//! if the tool mutates state — gated tools require user approval when the
//! chat is in manual mode. MCP tools are registered dynamically elsewhere
//! and are ALWAYS gated (unknown side effects).

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::rpc;
use crate::app::App;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub gated: bool,
    pub schema: fn() -> Value,
    pub run: fn(&Arc<App>, &Value) -> Result<Value>,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

pub fn registry() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "get_schema",
            description: "Get all table definitions and row counts in the app database.",
            gated: false,
            schema: || obj(json!({}), &[]),
            run: |app, _input| {
                let tables = rpc::dispatch(app, "sql.query", json!({
                    "sql": "SELECT name, sql FROM sqlite_master WHERE type IN ('table','view','index') AND name NOT LIKE 'sqlite_%'"
                }))?;
                let names = rpc::dispatch(app, "sql.query", json!({
                    "sql": "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'uapp_%' AND name != 'sqlar'"
                }))?;
                let mut counts = serde_json::Map::new();
                if let Some(rows) = names["rows"].as_array() {
                    for r in rows {
                        if let Some(t) = r[0].as_str() {
                            if let Ok(c) = rpc::dispatch(app, "sql.query",
                                json!({"sql": format!("SELECT count(*) FROM \"{}\"", t)})) {
                                counts.insert(t.to_string(), c["rows"][0][0].clone());
                            }
                        }
                    }
                }
                Ok(json!({"schema": tables, "row_counts": counts}))
            },
        },
        ToolDef {
            name: "sql_query",
            description: "Run read-only SQL (SELECT/WITH/PRAGMA) against the app database. Complex queries, joins and aggregates are encouraged.",
            gated: false,
            schema: || obj(json!({"sql": {"type": "string"}, "params": {"type": "array"}}), &["sql"]),
            run: |app, input| rpc::dispatch(app, "sql.query", input.clone()),
        },
        ToolDef {
            name: "sql_exec",
            description: "Run ONE deterministic write statement (INSERT/UPDATE/DELETE/DDL) with optional params. Replicated to all users.",
            gated: true,
            schema: || obj(json!({"sql": {"type": "string"}, "params": {"type": "array"}}), &["sql"]),
            run: |app, input| rpc::dispatch(app, "sql.exec", input.clone()),
        },
        ToolDef {
            name: "sql_batch",
            description: "Run multiple write statements separated by semicolons as one atomic write (schema setup, multi-step transforms like CREATE TABLE x AS SELECT...).",
            gated: true,
            schema: || obj(json!({"sql": {"type": "string"}}), &["sql"]),
            run: |app, input| rpc::dispatch(app, "sql.batch", input.clone()),
        },
        ToolDef {
            name: "list_files",
            description: "List files stored in the app archive.",
            gated: false,
            schema: || obj(json!({}), &[]),
            run: |app, _input| rpc::dispatch(app, "files.list", json!({})),
        },
        ToolDef {
            name: "read_file",
            description: "Read a file from the app archive (returns text when valid UTF-8).",
            gated: false,
            schema: || obj(json!({"name": {"type": "string"}}), &["name"]),
            run: |app, input| {
                let r = rpc::dispatch(app, "files.read", input.clone())?;
                match r["text"].as_str() {
                    Some(t) => Ok(json!({"name": r["name"], "text": t})),
                    None => Ok(json!({"name": r["name"], "note": "binary file",
                        "bytes": r["b64"].as_str().map(|b| b.len() * 3 / 4)})),
                }
            },
        },
        ToolDef {
            name: "write_file",
            description: "Create or overwrite a text file in the app archive (e.g. index.html, js/app.js — those land under app/; user data goes under data/). Previous version is kept in history. The app iframe reloads automatically.",
            gated: true,
            schema: || obj(json!({"name": {"type": "string"}, "content": {"type": "string"}}),
                           &["name", "content"]),
            run: |app, input| rpc::dispatch(app, "files.write",
                json!({"name": input["name"], "text": input["content"]})),
        },
        ToolDef {
            name: "edit_file",
            description: "Edit a text file in the archive by exact string replacement — PREFER this over write_file for any change to an existing file (write_file resends the whole file). old_string must match the file byte-for-byte, whitespace included, and appear exactly once — include enough surrounding lines to make it unique — or set replace_all to change every occurrence. Previous version is kept in history. The app iframe reloads automatically.",
            gated: true,
            schema: || obj(json!({
                "name": {"type": "string"},
                "old_string": {"type": "string", "description": "exact existing text to replace (must be unique in the file unless replace_all)"},
                "new_string": {"type": "string", "description": "the replacement text"},
                "replace_all": {"type": "boolean", "description": "replace every occurrence (default false)"}
            }), &["name", "old_string", "new_string"]),
            run: |app, input| {
                let old = input["old_string"].as_str().unwrap_or("");
                let new = input["new_string"].as_str().unwrap_or("");
                if old.is_empty() {
                    anyhow::bail!("old_string is empty — to create a file, use write_file");
                }
                if old == new {
                    anyhow::bail!("old_string and new_string are identical — nothing to change");
                }
                let r = rpc::dispatch(app, "files.read", json!({"name": input["name"]}))?;
                let name = r["name"].as_str().unwrap_or_default().to_string();
                let Some(text) = r["text"].as_str() else {
                    anyhow::bail!("{name} is a binary file — edit_file only works on text");
                };
                let n = text.matches(old).count();
                if n == 0 {
                    anyhow::bail!("old_string not found in {name} — read the file and copy the text exactly (whitespace included)");
                }
                let replace_all = input["replace_all"].as_bool().unwrap_or(false);
                if n > 1 && !replace_all {
                    anyhow::bail!("old_string appears {n} times in {name} — add surrounding context to make it unique, or set replace_all");
                }
                let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
                rpc::dispatch(app, "files.write", json!({"name": name, "text": updated}))?;
                Ok(json!({"name": name, "replacements": n, "size": updated.len()}))
            },
        },
        ToolDef {
            name: "delete_file",
            description: "Delete a file from the app archive.",
            gated: true,
            schema: || obj(json!({"name": {"type": "string"}}), &["name"]),
            run: |app, input| rpc::dispatch(app, "files.delete", input.clone()),
        },
        ToolDef {
            name: "run_js",
            description: "Execute JavaScript in the browser. context \"scratchpad\" (default): a hidden empty page with the full uapp API (uapp.query/exec/readFile/tool/...) and a loadScript(url) helper — globals persist between calls; ideal for parsing uploaded files (Excel via SheetJS), transforming data, testing logic. context \"app\": runs inside the LIVE app page — its DOM, globals, and uapp.call(...) actions. The code is the body of an async function; use `return` for the result (must be JSON-serializable). console output is captured.",
            gated: true,
            schema: || obj(json!({
                "code": {"type": "string", "description": "async function body; `return` the result"},
                "context": {"type": "string", "enum": ["scratchpad", "app"],
                             "description": "where to run (default scratchpad)"}
            }), &["code"]),
            run: |app, input| rpc::dispatch(app, "eval.run", input.clone()),
        },
        ToolDef {
            name: "read_console",
            description: "Read recent console output and uncaught errors (with stack traces) from the LIVE app page — the app's real runtime logs, not just code you ran via run_js. Use this to debug: after write_file/edit_file (the iframe reloads and may throw on load) or after a run_js that drives the UI, and whenever a turn ends with a '[console] …' alert. Defaults to the latest page load only (so stale pre-edit errors don't show); set only_latest=false for older lines, only_errors=true for just errors/warnings.",
            gated: false,
            schema: || obj(json!({
                "only_errors": {"type": "boolean", "description": "only error/warn lines (default false)"},
                "only_latest": {"type": "boolean", "description": "only the most recent page load (default true)"},
                "limit": {"type": "integer", "description": "max lines, newest kept (default 100, max 500)"}
            }), &[]),
            run: |app, input| rpc::dispatch(app, "console.read", input.clone()),
        },
        ToolDef {
            name: "present_file",
            description: "Show a file from the app archive to the user: it opens in their file viewer right away (images, video, audio, PDF, markdown rendered, code with highlighting). Use it whenever you produce or find a file worth looking at — a report you wrote, a chart, a document, a photo, a code file you changed. Display only: nothing is modified. mode \"pretty\" renders markdown, \"raw\" shows the source, \"edit\" opens the editor, \"auto\" (default) picks per file type.",
            gated: false,
            schema: || obj(json!({
                "name": {"type": "string", "description": "archive path, e.g. app/reports/q3.md or data/sales.csv"},
                "mode": {"type": "string", "enum": ["auto", "raw", "pretty", "edit"],
                          "description": "how to open it (default auto)"}
            }), &["name"]),
            run: |app, input| rpc::dispatch(app, "files.present",
                json!({"name": input["name"], "mode": input["mode"], "_assistant": true})),
        },
        // Sub-agents: ungated. The inner loop runs every tool through the same
        // approval_gate with the parent's mode, so gated work inside still
        // prompts — a second prompt for the delegation itself buys nothing.
        ToolDef {
            name: "agent_run",
            description: "Delegate a big, self-contained task to an autonomous sub-agent. It works in its own conversation with the same tools you have, then hands back a written report — so a long research/audit/build job doesn't fill up this conversation. Give it a fully self-contained prompt: it cannot ask questions. mode \"fresh\" (default) starts clean; \"fork\" prefixes a digest of this conversation. Returns {session, steps, report}.",
            gated: false,
            schema: || obj(json!({
                "description": {"type": "string", "description": "short label, e.g. 'audit invoice tables'"},
                "prompt": {"type": "string", "description": "the complete, self-contained task"},
                "mode": {"type": "string", "enum": ["fresh", "fork"],
                          "description": "fresh (default) or fork this conversation's context"},
                "max_steps": {"type": "integer", "description": "tool-loop steps, default 12, max 24"}
            }), &["description", "prompt"]),
            run: |app, input| crate::ai::agent_run(app, input),
        },
        ToolDef {
            name: "agent_send",
            description: "Send a follow-up instruction to a sub-agent conversation started with agent_run (use the session id it returned). Runs its loop again and returns the new report.",
            gated: false,
            schema: || obj(json!({
                "session": {"type": "string", "description": "agent session id from agent_run"},
                "message": {"type": "string"}
            }), &["session", "message"]),
            run: |app, input| crate::ai::agent_send(app, input),
        },
        ToolDef {
            name: "ask_user",
            description: "Ask the user 1-4 questions and wait for their answers. Each question renders its options as radio buttons (or checkboxes with multiSelect) and ALWAYS offers a free-form 'Other' field, so give concrete options — if you recommend one, put it first and append ' (Recommended)' to its label. Use it when a decision genuinely belongs to the user (destructive changes, ambiguous requirements, taste); don't use it for choices with an obvious default. Returns {answered, answers: [{question, choices, other?}]} — unanswered prompts time out, so proceed on your best judgment then.",
            gated: false,
            schema: || obj(json!({
                "questions": {"type": "array", "minItems": 1, "maxItems": 4, "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "description": "the complete question, ending with a question mark"},
                        "header": {"type": "string", "description": "very short chip label, max 12 chars (e.g. 'Approach')"},
                        "options": {"type": "array", "items": {"type": "object", "properties": {
                            "label": {"type": "string", "description": "concise choice text (1-5 words)"},
                            "description": {"type": "string", "description": "what this option means / its trade-offs"}
                        }, "required": ["label"]}, "description": "2-12 distinct choices (omit for pure free-form)"},
                        "multiSelect": {"type": "boolean", "description": "allow selecting several options (checkboxes)"}
                    },
                    "required": ["question"]
                }}
            }), &["questions"]),
            run: |app, input| crate::ai::ask_user(app, input),
        },
        ToolDef {
            name: "web_search",
            description: "Search the web (DuckDuckGo, no API key). Returns titles, URLs and snippets. Use for current information, reference data, docs.",
            gated: false,
            schema: || obj(json!({
                "query": {"type": "string"},
                "count": {"type": "integer", "description": "max results, default 6"}
            }), &["query"]),
            run: |app, input| rpc::dispatch(app, "web.search", input.clone()),
        },
        ToolDef {
            name: "fetch_url",
            description: "Fetch a web page and return its readable text (HTML is stripped, 20KB cap). Use after web_search to read a result, or on any URL the user gives you.",
            gated: false,
            schema: || obj(json!({"url": {"type": "string"}}), &["url"]),
            run: |app, input| rpc::dispatch(app, "web.fetch", input.clone()),
        },
        ToolDef {
            name: "add_mcp_server",
            description: "Connect a remote MCP server for this app (verified before saving; its tools appear immediately as mcp__<name>__* and the config replicates to the whole team).",
            gated: true,
            schema: || obj(json!({
                "name": {"type": "string", "description": "short identifier, e.g. 'crm'"},
                "url": {"type": "string", "description": "https URL of the MCP endpoint"},
                "bearer_token": {"type": "string", "description": "optional Authorization bearer token"}
            }), &["name", "url"]),
            run: |app, input| rpc::dispatch(app, "mcp.add", input.clone()),
        },
        ToolDef {
            name: "remove_mcp_server",
            description: "Disconnect a previously added MCP server by name.",
            gated: true,
            schema: || obj(json!({"name": {"type": "string"}}), &["name"]),
            run: |app, input| rpc::dispatch(app, "mcp.remove", input.clone()),
        },
        ToolDef {
            name: "download_lib",
            description: "Vendor a JS/CSS/WASM library or asset into the app archive by downloading it from a URL (e.g. cdn.jsdelivr.net, unpkg.com, cdn.sheetjs.com). Stored under app/vendor/ by default, served same-origin so it works offline and WASM can instantiateStreaming. Reference it with a relative path afterward.",
            gated: true,
            schema: || obj(json!({
                "url": {"type": "string", "description": "http(s) URL of the file to download"},
                "name": {"type": "string", "description": "archive path to store it at (default: app/vendor/<filename from url>)"}
            }), &["url"]),
            run: |app, input| rpc::dispatch(app, "files.fetch", input.clone()),
        },
        ToolDef {
            name: "import_csv",
            description: "Parse a CSV/TSV file already in the archive (usually data/...) into a database table as one efficient bulk write. Auto-detects delimiter and column types.",
            gated: true,
            schema: || obj(json!({
                "file": {"type": "string", "description": "archive path, e.g. data/sales.csv"},
                "table": {"type": "string", "description": "target table name (default: from filename)"},
                "header": {"type": "boolean", "description": "first row is headers (default true)"},
                "replace": {"type": "boolean", "description": "delete existing rows first"}
            }), &["file"]),
            run: |app, input| rpc::dispatch(app, "import.csv", input.clone()),
        },
    ]
}

/// The delegation tools. They are removed from a sub-agent's own toolset,
/// which is what keeps the agent tree one level deep.
pub fn is_agent_tool(name: &str) -> bool {
    name == "agent_run" || name == "agent_send"
}

/// Names in config key "tools_disabled" (synced app-wide).
pub fn disabled_set(app: &Arc<App>) -> std::collections::HashSet<String> {
    let eng = app.engine.lock().unwrap();
    crate::store::config_get(&eng.db, "tools_disabled")
        .ok()
        .flatten()
        .and_then(|v| v.as_array().cloned())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

/// Calls that reach outside the app's own database/archive — a local database
/// file (ATTACH) or a local/private network host (e.g. controlling OBS on
/// 127.0.0.1) — always require an explicit user prompt, in auto mode too.
/// Returns (always_allow_key, reason). The key is narrower than the tool name
/// so "always allow" only covers this kind of access, and the reason is shown
/// on the prompt.
pub fn forced_gate(name: &str, input: &Value) -> Option<(String, String)> {
    match name {
        "sql_query" => {
            let sql = input["sql"].as_str().unwrap_or("");
            crate::store::mentions_attach(sql).then(|| (
                "sql_query:attach".to_string(),
                "attaches another database file on this machine".to_string(),
            ))
        }
        "fetch_url" | "download_lib" => {
            let url = input["url"].as_str().unwrap_or("");
            match crate::rpc::check_public_url(url) {
                Ok(()) => None,
                Err(_) => Some((
                    format!("{name}:local"),
                    "contacts a local/private network address on this machine".to_string(),
                )),
            }
        }
        _ => None,
    }
}

/// Is this tool gated (needs approval in manual mode)? MCP tools always are;
/// app actions are gated unless the app declared them readonly.
pub fn is_gated(app: &Arc<App>, name: &str) -> bool {
    if name.starts_with("mcp__") {
        return true;
    }
    if let Some(action) = name.strip_prefix("app__") {
        return app
            .actions
            .lock()
            .unwrap()
            .get(action)
            .map(|a| !a.readonly)
            .unwrap_or(true);
    }
    registry().iter().any(|t| t.name == name && t.gated)
}
