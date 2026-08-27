//! JSON-RPC method implementations. Shared by the shell UI, embedded apps
//! (via /uapp.js) and the AI tool loop — everyone gets the same API surface.

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::app::App;
use crate::store;

pub fn dispatch(app: &Arc<App>, method: &str, p: Value) -> Result<Value> {
    match method {
        "app.info" => {
            let eng = app.engine.lock().unwrap();
            let name = store::meta_get(&eng.db, "name")?.unwrap_or_default();
            Ok(json!({
                "name": name,
                "device": eng.device,
                "user": eng.user,
                "appId": eng.app_id,
                // Any session's run is live (compat); aiSessions names them.
                "aiBusy": app.ai_busy_any(),
                "aiSessions": app.ai_sessions(),
                "unsaved": app.unsaved.load(Ordering::Relaxed),
                // Absolute path of the .uapp checkpoint (used by the mobile
                // "add to launcher" shortcut bridge).
                "path": eng.path.display().to_string(),
                // True when embedded in the native desktop window: the shell
                // then shows custom window controls + a native "Save…" dialog.
                "native": crate::native::is_native(),
                // Desktop build? On mobile the native window has no min/max/
                // drag (and no chrome to drag), so the shell hides those.
                "desktop": cfg!(desktop),
                // Is an AI provider configured? Drives the chat panel's
                // "configure a provider" setup banner.
                "aiConfigured": store::config_get(&eng.db, "ai").ok().flatten()
                    .and_then(|v| v["provider"].as_str().map(|p| !p.is_empty()))
                    .unwrap_or(false),
                // At-rest encryption on for this file? Drives the Settings lock UI.
                "encrypted": eng.passphrase().is_some(),
                // Still the default app (not built yet)? The shell opens the AI
                // chat sidebar for a new app, and keeps it closed otherwise.
                "blank": store::is_blank_app(&eng.db),
                // On-disk size of the .uapp (Settings → Maintenance).
                "fileSize": file_size(&eng),
                // Browser wasm demo build? The shell hides what can't work there.
                "wasm": cfg!(target_arch = "wasm32"),
            }))
        }
        "sql.query" => {
            let eng = app.engine.lock().unwrap();
            let params: Vec<Value> = p["params"].as_array().cloned().unwrap_or_default();
            // _user_approved is set only by the AI loop after the user's
            // permission prompt (and stripped from wire requests).
            if p["_user_approved"] == json!(true) {
                store::query_allow_attach(&eng.db, need_str(&p, "sql")?, &params)
            } else {
                store::query(&eng.db, need_str(&p, "sql")?, &params)
            }
        }
        "sql.exec" => {
            let r = local_op(app, "sql", json!({"sql": need_str(&p, "sql")?, "params": p["params"]}))?;
            Ok(r)
        }
        "sql.batch" => local_op(app, "batch", json!({"sql": need_str(&p, "sql")?})),
        "files.list" => {
            let eng = app.engine.lock().unwrap();
            let mut r = store::query(
                &eng.db,
                "SELECT name, sz, mtime FROM sqlar ORDER BY name",
                &[],
            )?;
            // Role is the top-level directory the file sits in (app/ vs data/);
            // kept as a column so callers don't have to parse paths.
            r["columns"].as_array_mut().map(|c| c.push(json!("role")));
            if let Some(rows) = r["rows"].as_array_mut() {
                for row in rows {
                    let role = row[0].as_str().map(store::file_role).unwrap_or("app");
                    row.as_array_mut().map(|a| a.push(json!(role)));
                }
            }
            Ok(r)
        }
        "files.read" => {
            let eng = app.engine.lock().unwrap();
            let name = need_str(&p, "name")?;
            // Report the archive name it resolved to, not the reference given.
            let name = &store::resolve_name(&eng.db, name)?.unwrap_or_else(|| name.to_string());
            match store::sqlar_read(&eng.db, name)? {
                Some(data) => Ok(json!({
                    "name": name,
                    "b64": base64::engine::general_purpose::STANDARD.encode(&data),
                    "text": String::from_utf8(data.clone()).ok(),
                })),
                None => bail!("no such file: {name}"),
            }
        }
        "files.write" => {
            // Canonical name in the op payload, so every replica (and the
            // response) agrees on where the bytes went.
            let name = store::canon_name(need_str(&p, "name")?);
            // canon_name("") / canon_name("dir/") land on a bare directory.
            if name.ends_with('/') {
                bail!("a file needs a name (and it can't end with '/')");
            }
            let b64 = if let Some(b) = p["b64"].as_str() {
                b.to_string()
            } else if let Some(t) = p["text"].as_str() {
                base64::engine::general_purpose::STANDARD.encode(t.as_bytes())
            } else {
                bail!("files.write needs b64 or text");
            };
            local_op(app, "file_put", json!({"name": name, "data": b64}))?;
            Ok(json!({"ok": true, "name": name}))
        }
        "files.delete" => {
            // A miss is an error, never a silent no-op: "deleted" must mean gone.
            let name = {
                let eng = app.engine.lock().unwrap();
                let asked = need_str(&p, "name")?;
                store::resolve_name(&eng.db, asked)?
                    .ok_or_else(|| anyhow!("no file named '{asked}' (list_files shows the archive's names)"))?
            };
            local_op(app, "file_del", json!({"name": name}))
        }
        "files.rename" => files_rename(app, p),
        // Display-only: ask every open shell to show this file. Not an op —
        // nothing about the app changes, so nothing is written
        // beyond this session's clients.
        "files.present" => {
            let mut name = need_str(&p, "name")?.to_string();
            let mode = p["mode"].as_str().unwrap_or("auto").to_string();
            if !matches!(mode.as_str(), "auto" | "raw" | "pretty" | "edit") {
                bail!("mode must be auto, raw, pretty or edit");
            }
            let by = {
                let eng = app.engine.lock().unwrap();
                match store::resolve_name(&eng.db, &name)? {
                    Some(n) => name = n,
                    None => bail!("no such file: {name}"),
                }
                // `_assistant` is set by the present_file tool (stripped from
                // wire requests), so a user-driven present is attributed to them.
                if p["_assistant"] == json!(true) {
                    "assistant".to_string()
                } else {
                    eng.user.clone()
                }
            };
            app.notify("present", json!({"name": name, "mode": mode, "by": by}));
            Ok(json!({"ok": true}))
        }
        // Ask every shell to reload its app frame. Writes no longer do this
        // implicitly (see shell/main.js), so this is how a finished edit is
        // shown — the AI's `reload_app` tool, or any client.
        "app.reload" => {
            app.notify("reload", json!({}));
            Ok(json!({"ok": true}))
        }
        "files.fetch" => files_fetch(app, p),
        // Native-window file drops (see native.rs): the page passes back the id
        // it was notified with, never a path.
        "drop.ingest" => drop_ingest(app, p),
        // Dropping a template .uapp on the window updates THIS app's code from
        // it and leaves the data alone. Two steps on purpose: inspect stages
        // the file and reports what would change, apply commits it.
        "template.inspect" => template_inspect(app, p),
        "template.apply" => template_apply(app, p),
        "template.discard" => {
            app.templates.lock().unwrap().remove(need_str(&p, "token")?);
            Ok(json!({"ok": true}))
        }
        // Fabricate one, so the drop path can be tested without a desktop
        // session (the real producer is the native window's event handler).
        // Off unless UAPP_TEST_DROPS is set: it takes paths from the caller.
        "drop.simulate" if std::env::var("UAPP_TEST_DROPS").is_ok() => {
            let phase = match p["phase"].as_str().unwrap_or("drop") {
                "hover" => crate::native::DropPhase::Hover,
                "leave" => crate::native::DropPhase::Leave,
                _ => crate::native::DropPhase::Drop,
            };
            let paths = p["paths"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
                .collect();
            app.native_drop(crate::native::DropInfo {
                phase,
                paths,
                x: p["x"].as_f64().unwrap_or(0.0),
                y: p["y"].as_f64().unwrap_or(0.0),
            });
            Ok(json!({"ok": true}))
        }
        "files.history" => {
            let eng = app.engine.lock().unwrap();
            let name = need_str(&p, "name")?;
            let name = store::resolve_name(&eng.db, name)?
                .unwrap_or_else(|| store::canon_name(name));
            store::query(
                &eng.db,
                "SELECT id, ts, device, user, action, COALESCE(sz, length(data)) AS size
                 FROM uapp_file_history WHERE name=?1 ORDER BY id DESC",
                &[json!(name)],
            )
        }
        "files.revert" => {
            let id = p["id"].as_i64().ok_or_else(|| anyhow!("files.revert needs id"))?;
            let mut name = need_str(&p, "name")?.to_string();
            let (sz, data): (Option<i64>, Option<Vec<u8>>) = {
                let eng = app.engine.lock().unwrap();
                name = store::resolve_name(&eng.db, &name)?
                    .unwrap_or_else(|| store::canon_name(&name));
                let mut stmt = eng.db.prepare(
                    "SELECT sz, data FROM uapp_file_history WHERE id=?1 AND name=?2",
                )?;
                let mut rows = stmt.query(rusqlite::params![id, name])?;
                match rows.next()? {
                    Some(row) => (row.get(0)?, row.get(1)?),
                    None => bail!("no such history entry"),
                }
            };
            match data {
                Some(d) => {
                    // Snapshots are stored sqlar-style (sz NULL = legacy raw).
                    let d = store::sqlar_inflate(sz, d, &name)?;
                    local_op(
                        app,
                        "file_put",
                        json!({"name": name,
                               "data": base64::engine::general_purpose::STANDARD.encode(&d)}),
                    )
                }
                None => bail!("that history entry has no content (file was created then)"),
            }
        }
        "config.get" => {
            let eng = app.engine.lock().unwrap();
            Ok(store::config_get(&eng.db, need_str(&p, "key")?)?.unwrap_or(Value::Null))
        }
        "config.set" => local_op(
            app,
            "config_set",
            json!({"key": need_str(&p, "key")?, "value": p["value"]}),
        ),
        "chat.list" => {
            let session = session_arg(&p)?;
            let eng = app.engine.lock().unwrap();
            store::query(
                &eng.db,
                "SELECT mid, ts, device, user, role, content, COALESCE(session,'main') AS session
                 FROM uapp_chat WHERE COALESCE(session,'main') = ?1
                 ORDER BY ts, ctr, device, id",
                &[json!(session)],
            )
        }
        "chat.send" => chat_send(app, p),
        "chat.clear" => local_op(app, "chat_clear", json!({"session": session_arg(&p)?})),
        "chat.compact" => {
            let session = session_arg(&p)?;
            crate::ai::compact_session(app, &session)
        }
        "sessions.list" => sessions_list(app),
        "sessions.create" => {
            let id = format!("s-{}-{}", crate::store::now_ms(), fastrand::u32(..));
            let title = p["title"].as_str().unwrap_or("New chat").trim().to_string();
            let title = if title.is_empty() { "New chat".to_string() } else { title };
            local_op(app, "session_meta", json!({"id": id, "title": title, "kind": "chat",
                                                 "created": crate::store::now_ms()}))?;
            Ok(json!({"id": id, "title": title}))
        }
        "sessions.rename" => {
            let id = session_id(need_str(&p, "id")?)?;
            let title = need_str(&p, "title")?.trim().to_string();
            if title.is_empty() || title.len() > 200 {
                bail!("chat title must be 1-200 characters");
            }
            local_op(app, "session_meta", json!({"id": id, "title": title,
                                                 "created": crate::store::now_ms()}))?;
            Ok(json!({"ok": true, "id": id, "title": title}))
        }
        "sessions.archive" => {
            let id = session_id(need_str(&p, "id")?)?;
            let archived = p["archived"].as_bool().unwrap_or(true);
            local_op(app, "session_meta", json!({"id": id, "archived": archived,
                                                 "created": crate::store::now_ms()}))?;
            Ok(json!({"ok": true, "id": id, "archived": archived}))
        }
        "sessions.delete" => {
            let id = session_id(need_str(&p, "id")?)?;
            // A run still writing into it would resurrect rows after the op.
            app.ai_stop(Some(&id));
            local_op(app, "session_del", json!({"id": id}))?;
            Ok(json!({"ok": true, "id": id}))
        }
        "ai.stop" => {
            // No session = stop everything (what the old wire shape meant).
            let session = p["session"].as_str().map(|s| s.to_string());
            app.ai_stop(session.as_deref());
            Ok(json!({"ok": true}))
        }
        "ai.pending" => Ok(app.pending_list()),
        "ai.questions" => Ok(app.questions_list()),
        // Answer an ask_user prompt: {"id", "answers": [{choices:[..], other:".."}]},
        // one entry per question in the prompt's order.
        "ai.answer" => {
            let id = need_str(&p, "id")?;
            let entry = app.questions.lock().unwrap().remove(id);
            match entry {
                Some(e) => {
                    let _ = e.tx.try_send(json!({
                        "answers": p["answers"].as_array().cloned().unwrap_or_default(),
                    }));
                    app.notify_questions();
                    Ok(json!({"ok": true}))
                }
                None => bail!("no open question with id {id}"),
            }
        }
        "ai.approve" => {
            let id = need_str(&p, "id")?;
            let allow = p["allow"].as_bool().unwrap_or(false);
            let always = p["always"].as_bool().unwrap_or(false);
            let entry = app.pending.lock().unwrap().remove(id);
            match entry {
                Some(e) => {
                    let _ = e.tx.try_send((allow, always));
                    app.notify_pending();
                    Ok(json!({"ok": true}))
                }
                None => bail!("no pending approval with id {id}"),
            }
        }
        "tools.list" => {
            let disabled = crate::tools::disabled_set(app);
            let mut out: Vec<Value> = crate::tools::registry()
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description,
                    "gated": t.gated, "kind": "builtin",
                    "enabled": !disabled.contains(t.name)}))
                .collect();
            for (name, a) in app.actions_snapshot() {
                let tool_name = format!("app__{name}");
                out.push(json!({"name": tool_name,
                    "description": a.description, "gated": !a.readonly,
                    "kind": "app",
                    "enabled": !disabled.contains(&tool_name)}));
            }
            for t in crate::mcp::list_all_tools(&mcp_servers_enabled(app)) {
                let name = format!("mcp__{}__{}",
                    t["server"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""));
                out.push(json!({"name": name,
                    "description": t["description"], "gated": true,
                    "kind": "mcp", "server": t["server"],
                    "enabled": !disabled.contains(&name)}));
            }
            Ok(json!(out))
        }
        // Symmetry: the app can call every tool the AI can, through the same
        // executor (builtins, MCP passthrough, other app actions). Gated tools
        // require user approval (with "always allow" option) unless previously
        // approved for this app.
        "tools.call" => {
            let name = need_str(&p, "name")?.to_string();
            if crate::tools::disabled_set(app).contains(&name) {
                bail!("tool '{name}' is disabled in this app's settings");
            }
            // Check if this is a gated tool and requires approval for app-triggered calls
            // Let a freshly reloaded page finish registering before deciding
            // whether the action is gated (its readonly flag lives on the
            // registration). Gating itself lives in one place: tools::is_gated.
            if let Some(action) = name.strip_prefix("app__") {
                app.wait_for_action(action)?;
            }
            if crate::tools::is_gated(app, &name) {
                crate::ai::app_approval_gate(app, &name, &p["input"])?;
            }
            crate::ai::run_tool(app, &name, &p["input"])
        }
        "ai.run" => {
            // Re-run the assistant on the existing conversation (e.g. after
            // setting up a provider when earlier sends went unanswered).
            spawn_ai(app.clone(), session_arg(&p)?);
            Ok(json!({"ok": true}))
        }
        "import.csv" => import_csv(app, p),
        "app.rename" => {
            let name = need_str(&p, "name")?.trim().to_string();
            if name.is_empty() || name.len() > 80 {
                bail!("app name must be 1-80 characters");
            }
            local_op(app, "sql", json!({
                "sql": "INSERT OR REPLACE INTO uapp_meta(key,value) VALUES('name',?1)",
                "params": [name]
            }))?;
            app.notify("renamed", json!({"name": name}));
            Ok(json!({"ok": true, "name": name}))
        }
        "app.quit" => {
            // Close the app: tell clients (browser tabs show an overlay; the
            // native window is closed by the server itself), then exit
            // cleanly. All state is already committed to the file.
            app.notify("sync", json!({"state": "closed"}));
            #[cfg(not(target_arch = "wasm32"))]
            {
                let app2 = app.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    app2.graceful_cleanup();
                    eprintln!("uapp: closed from the shell");
                    crate::native::dispatch(crate::native::NativeReq::Close);
                    std::process::exit(0);
                });
            }
            Ok(json!({"ok": true}))
        }
        // Reclaim dead space in the .uapp (deleted rows/files keep their pages
        // until a VACUUM) and refresh the rescue snapshot.
        "app.compact" => {
            let mut eng = app.engine.lock().unwrap();
            eng.db.execute_batch("VACUUM")?;
            let _ = eng.snapshot();
            let size = file_size(&eng);
            Ok(json!({"ok": true, "fileSize": size}))
        }
        "mcp.tools" => {
            let servers = mcp_servers_enabled(app);
            Ok(json!(crate::mcp::list_all_tools(&servers)))
        }
        "mcp.status" => Ok(mcp_status(app)),
        "mcp.add" => mcp_add(app, p),
        "mcp.remove" => {
            let name = need_str(&p, "name")?.to_string();
            mcp_mutate(app, |servers| servers.retain(|s| s["name"].as_str() != Some(&name)))
        }
        "mcp.toggle" => {
            let name = need_str(&p, "name")?.to_string();
            let enabled = p["enabled"].as_bool().unwrap_or(true);
            mcp_mutate(app, |servers| {
                for s in servers.iter_mut() {
                    if s["name"].as_str() == Some(&name) {
                        s["enabled"] = json!(enabled);
                    }
                }
            })
        }
        "eval.run" => {
            let code = need_str(&p, "code")?;
            let context = p["context"].as_str().unwrap_or("scratchpad");
            if context != "scratchpad" && context != "app" {
                bail!("context must be \"scratchpad\" or \"app\"");
            }
            app.invoke_eval(context, code)
        }
        // Recent console output / uncaught errors from the live app iframe,
        // forwarded over /ws and buffered on App. Read by the read_console tool.
        "console.read" => {
            let only_errors = p["only_errors"].as_bool().unwrap_or(false);
            // Default to just the latest page load: after an edit the iframe
            // reloads, and stale pre-fix errors are noise.
            let only_latest = p["only_latest"].as_bool().unwrap_or(true);
            let limit = p["limit"].as_u64().unwrap_or(100).clamp(1, 500) as usize;
            Ok(app.console_read(only_errors, only_latest, limit))
        }
        // Machine-local prefs (NOT synced) — e.g. browser vs. native window.
        "prefs.get" => Ok(crate::prefs::load()),
        "prefs.set" => {
            crate::prefs::set(need_str(&p, "key")?, p["value"].clone())?;
            Ok(json!({"ok": true}))
        }
        // Web permissions. READ-ONLY from here on purpose: the shell and the
        // app iframe share an origin, so any RPC that could grant a permission
        // is an RPC untrusted app code can call on itself. Granting happens
        // only through the native dialog in `gui.rs`; there is deliberately no
        // `perm.setGrant` and no `perm.answer`.
        #[cfg(not(target_arch = "wasm32"))]
        "perm.grants" => {
            let app_id = app.engine.lock().unwrap().app_id.clone();
            Ok(json!({"appId": app_id, "grants": crate::permissions::summary(&app_id)}))
        }
        // Forgetting grants only ever removes privilege, but it can still be
        // abused — clearing a denial buys another prompt — so it is confirmed
        // in a native dialog the page cannot draw or click.
        #[cfg(not(target_arch = "wasm32"))]
        "perm.clearGrants" => {
            let app_id = app.engine.lock().unwrap().app_id.clone();
            if !crate::native::confirm_reset_permissions() {
                return Ok(json!({"ok": false, "cancelled": true}));
            }
            crate::permissions::clear_app(&app_id)?;
            Ok(json!({"ok": true}))
        }
        "web.search" => web_search(p),
        "web.fetch" => web_fetch(p),
        // Open a URL in the system browser (from apps or AI tools).
        "browser.open" => {
            let url = need_str(&p, "url")?;
            #[cfg(not(target_arch = "wasm32"))]
            open::that(url).map_err(|e| anyhow!("failed to open URL: {e}"))?;
            // wasm: the shell opens it (window.open) on this notification.
            #[cfg(target_arch = "wasm32")]
            app.notify("open_url", json!({"url": url}));
            Ok(json!({"ok": true}))
        }
        // Native window controls (no-ops in the browser; the shell hides the
        // buttons there). Routed to the Tauri shell via the native bridge.
        "win.minimize" => {
            crate::native::dispatch(crate::native::NativeReq::Minimize);
            Ok(json!({"ok": true}))
        }
        "win.maximize" => {
            crate::native::dispatch(crate::native::NativeReq::ToggleMaximize);
            Ok(json!({"ok": true}))
        }
        "win.close" => {
            crate::native::dispatch(crate::native::NativeReq::Close);
            Ok(json!({"ok": true}))
        }
        "win.drag" => {
            crate::native::dispatch(crate::native::NativeReq::StartDrag);
            Ok(json!({"ok": true}))
        }
        #[cfg(not(target_arch = "wasm32"))]
        "app.save" => app_save(app),
        #[cfg(target_arch = "wasm32")]
        "app.save" => bail!("Save is only available in the desktop app — use Download here"),
        // Pick another .uapp in a native dialog and open it. The way in for a
        // machine with no .uapp file association: nothing to double-click, so
        // without this a bare launch is a dead end.
        #[cfg(not(target_arch = "wasm32"))]
        "app.openFile" => app_open_file(app),
        #[cfg(target_arch = "wasm32")]
        "app.openFile" => bail!("Opening a .uapp file is only available in the desktop app"),
        // Install this app as a launcher/start-menu entry on the machine the
        // server runs on: `uapp <file>` launched directly, no browser needed.
        #[cfg(target_arch = "wasm32")]
        "app.installShortcut" | "app.removeShortcut" => bail!("shortcuts are not available in the browser demo"),
        #[cfg(target_arch = "wasm32")]
        "app.shortcutStatus" => Ok(json!({"supported": false, "installed": false, "detail": Value::Null})),
        #[cfg(not(target_arch = "wasm32"))]
        "app.installShortcut" => {
            if app.unsaved.load(Ordering::Relaxed) {
                bail!("this app isn't saved to a file yet — download it first");
            }
            let (app_id, name, path) = {
                let eng = app.engine.lock().unwrap();
                let name = store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "uapp".into());
                (eng.app_id.clone(), name, eng.path.clone())
            };
            // Optional custom icon from the settings UI. Validated here so a
            // bad upload surfaces as a clean RPC error, not a broken launcher.
            let icon = match p["icon_b64"].as_str() {
                Some(b64) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|_| anyhow!("icon_b64 is not valid base64"))?;
                    if bytes.len() > 1024 * 1024 {
                        bail!("icon is too large (max 1 MB)");
                    }
                    if !bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                        bail!("icon must be a PNG image");
                    }
                    Some(bytes)
                }
                None => None,
            };
            crate::shortcut::install(&app_id, &name, &path, icon.as_deref())
                .map(|msg| json!({ "ok": true, "message": msg }))
        },
        // Is a launcher entry installed for this app on this machine? The
        // settings UI drives its install/remove buttons off this.
        #[cfg(not(target_arch = "wasm32"))]
        "app.shortcutStatus" => {
            let (app_id, name, path) = {
                let eng = app.engine.lock().unwrap();
                let name = store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "uapp".into());
                (eng.app_id.clone(), name, eng.path.clone())
            };
            // "Supported" also requires a stable file for the entry to point
            // at — an unsaved app has nowhere to launch from yet.
            let supported = crate::shortcut::supported()
                && !app.unsaved.load(Ordering::Relaxed)
                && path.is_absolute();
            let (installed, detail) = if supported {
                crate::shortcut::status(&app_id, &name)
            } else {
                (false, None)
            };
            Ok(json!({ "supported": supported, "installed": installed, "detail": detail }))
        }
        #[cfg(not(target_arch = "wasm32"))]
        "app.removeShortcut" => {
            let (app_id, name) = {
                let eng = app.engine.lock().unwrap();
                let name = store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "uapp".into());
                (eng.app_id.clone(), name)
            };
            crate::shortcut::remove(&app_id, &name).map(|msg| json!({ "ok": true, "message": msg }))
        }
        // The whole current app as .uapp bytes (base64) — OPFS auto-save and
        // the Download button.
        #[cfg(target_arch = "wasm32")]
        "app.export" => {
            let eng = app.engine.lock().unwrap();
            let bytes = eng.serialize_bytes()?;
            Ok(json!({"b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                      "bytes": bytes.len()}))
        }
        // Template export works on wasm too (in-memory build).
        #[cfg(target_arch = "wasm32")]
        "app.exportTemplate" => {
            let eng = app.engine.lock().unwrap();
            let name = store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "app".into());
            let bytes = store::export_template(&eng.db, &name)?;
            Ok(json!({"b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                      "bytes": bytes.len(), "name": name}))
        }
        // Liveness probe for the shell/app clients (background/resume recovery).
        "ping" => Ok(json!({ "ok": true })),
        "crypt.set" => crypt_set(app, p),
        other => bail!("unknown method {other}"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn file_size(eng: &crate::engine::Engine) -> u64 {
    std::fs::metadata(&eng.path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(target_arch = "wasm32")]
fn file_size(eng: &crate::engine::Engine) -> u64 {
    eng.serialize_bytes().map(|b| b.len() as u64).unwrap_or(0)
}

fn need_str<'a>(p: &'a Value, key: &str) -> Result<&'a str> {
    p[key]
        .as_str()
        .ok_or_else(|| anyhow!("missing required string param '{key}'"))
}

/// Change this app's at-rest encryption from the UI (Settings → Encryption).
/// `new_password` empty/absent means "remove encryption"; otherwise set it (a
/// plaintext app gets encrypted, an encrypted one is re-keyed). When the app is
/// already encrypted, the correct `current_password` must be supplied to make a
/// change (verified against the in-memory key). Applies to the file for every
/// device — peers keep their old key until they reopen.
fn crypt_set(app: &Arc<App>, p: Value) -> Result<Value> {
    let new_password = p["new_password"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let current = p["current_password"].as_str().unwrap_or("").to_string();

    let mut eng = app.engine.lock().unwrap();
    let is_encrypted = eng.passphrase().is_some();
    if !is_encrypted && new_password.is_none() {
        bail!("this app is not encrypted");
    }
    // To change or remove encryption you must prove you know the current
    // password (matches the CLI, and guards against accidental changes).
    if is_encrypted {
        if current.is_empty() {
            bail!("enter the current master password");
        }
        if eng.passphrase() != Some(current.as_str()) {
            bail!("current password is incorrect");
        }
    }
    if let Some(pw) = &new_password {
        if pw.len() < 4 {
            bail!("choose a master password of at least 4 characters");
        }
    }
    eng.reencrypt(new_password.clone())?;
    drop(eng);

    // The lock state changed for everyone on this device's session.
    app.notify("changes", json!({"config": true}));
    Ok(json!({"encrypted": new_password.is_some()}))
}

#[cfg(not(target_arch = "wasm32"))]
/// Native-app "Save…" / "Save As": show a native save dialog (seeded with the
/// app name), then re-home the live app onto the chosen file — future edits and
/// checkpoints land there, the unsaved banner clears, and the open window keeps
/// running (same server/port). A scratch ("Untitled") also adopts the file name
/// as its app name.
fn app_save(app: &Arc<App>) -> Result<Value> {
    if !crate::native::is_native() {
        bail!("Save is only available in the desktop app");
    }
    let was_unsaved = app.unsaved.load(Ordering::Relaxed);
    let default_name = {
        let eng = app.engine.lock().unwrap();
        let name = store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "app".into());
        let safe: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' })
            .collect();
        format!("{}.uapp", safe.trim())
    };
    let path = match crate::native::dispatch(crate::native::NativeReq::SaveDialog { default_name }) {
        Some(p) => std::path::PathBuf::from(p),
        None => return Ok(json!({"saved": false})), // user cancelled
    };
    // Re-home the running engine onto the chosen file, and move the reuse
    // advertisement to the new path (same server/port, new backing file).
    let old_path = {
        let mut eng = app.engine.lock().unwrap();
        let old_path = eng.path.clone();
        eng.save_as(path.clone())?;
        crate::registry::write_addr(
            &eng.path,
            app.port.load(Ordering::Relaxed),
            &app.token,
        );
        old_path
    };
    // A scratch / default document is CONSUMED by Save As: its content now
    // lives in the chosen file, so delete the old one (and its registry
    // records) — the next bare launch starts a fresh blank document.
    if was_unsaved && crate::registry::key(&old_path) != crate::registry::key(&path) {
        let _ = std::fs::remove_file(&old_path);
        crate::registry::forget(&old_path);
    }
    // It's a real, saved file now — drop the "not saved" banner.
    app.unsaved.store(false, Ordering::Relaxed);
    // A scratch adopts the file's name (so "Untitled" → "Budget" on save-as);
    // an already-named app keeps its name.
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if was_unsaved && !stem.is_empty() {
        let _ = local_op(
            app,
            "sql",
            json!({
                "sql": "INSERT OR REPLACE INTO uapp_meta(key,value) VALUES('name',?1)",
                "params": [stem]
            }),
        );
        app.notify("renamed", json!({"name": stem}));
    }
    Ok(json!({"saved": true, "path": path.to_string_lossy(), "name": stem}))
}

#[cfg(not(target_arch = "wasm32"))]
/// Desktop "Load uApp file": show a native open dialog and start the chosen
/// app. One app per process (the engine, registry advertisement and permission
/// grants all hang off this one file), so this launches ANOTHER instance rather
/// than re-homing the live window — this app keeps running untouched, and the
/// new one gets its own window, reusing an already-running server for that file
/// if there is one.
///
/// An untouched scratch window (the blank "My App" a bare launch opens) is just
/// the way in, not work: once the picked app has its own window, this one closes
/// rather than being left behind empty. Its file stays where it was, so the next
/// bare launch opens the same blank document again.
fn app_open_file(app: &Arc<App>) -> Result<Value> {
    if !crate::native::is_native() || !cfg!(desktop) {
        bail!("Opening a .uapp file is only available in the desktop app");
    }
    let path = match crate::native::dispatch(crate::native::NativeReq::OpenDialog) {
        Some(p) => std::path::PathBuf::from(p),
        None => return Ok(json!({"opened": false})), // user cancelled
    };
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    let (cur_path, blank) = {
        let eng = app.engine.lock().unwrap();
        (eng.path.clone(), store::is_blank_app(&eng.db))
    };
    // Picking the file this window already has would hand the new instance our
    // own server (registry reuse) and then — on the blank-scratch path below —
    // shut it down under them. It's already open; say so instead.
    if crate::registry::key(&path) == crate::registry::key(&cur_path) {
        bail!("{} is already open in this window", path.display());
    }
    // The running binary IS the desktop app, so it's also the thing that knows
    // how to open a file handed to it (see gui.rs's resolve_app_path); the
    // machine's "Open apps in" preference still decides window vs browser.
    let exe = std::env::current_exe()
        .map_err(|e| anyhow!("couldn't locate the UApp app on disk: {e}"))?;
    let mut child = std::process::Command::new(&exe)
        .arg(&path)
        .spawn()
        .map_err(|e| anyhow!("couldn't open {}: {e}", path.display()))?;
    // This window can outlive many opened apps; reap each child so closing one
    // doesn't leave a zombie behind in our process table.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    // Nothing in this window but the default app? It has served its purpose —
    // close it so opening a file doesn't leave an empty "My App" around. The
    // window close runs the server's usual graceful shutdown (see gui.rs); the
    // app we just launched is its own process and keeps going.
    let closed = blank && app.unsaved.load(Ordering::Relaxed);
    if closed {
        crate::native::dispatch(crate::native::NativeReq::Close);
    }
    Ok(json!({"opened": true, "path": path.to_string_lossy(), "closed": closed}))
}

/// Run + persist a local op, then push change notifications to all clients.
pub fn local_op(app: &Arc<App>, kind: &str, payload: Value) -> Result<Value> {
    let (result, op) = {
        let mut eng = app.engine.lock().unwrap();
        eng.local_op(kind, payload)?
    };
    let mut c = crate::engine::ChangeSet::default();
    c.note(&op.kind);
    app.notify_changes(c);
    Ok(result)
}

pub fn mcp_servers(app: &Arc<App>) -> Vec<Value> {
    let eng = app.engine.lock().unwrap();
    store::config_get(&eng.db, "mcp")
        .ok()
        .flatten()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// Only servers not switched off — what the toolset is built from.
pub fn mcp_servers_enabled(app: &Arc<App>) -> Vec<Value> {
    mcp_servers(app)
        .into_iter()
        .filter(|s| s["enabled"].as_bool().unwrap_or(true))
        .collect()
}

fn chat_send(app: &Arc<App>, p: Value) -> Result<Value> {
    let session = session_arg(&p)?;
    let text = p["text"].as_str().unwrap_or("").to_string();
    // Permission mode for this turn: "auto" (default), "manual" (gated tools
    // ask first), or "plan" (read-only toolset — investigate and propose).
    if let Some(mode) = p["mode"].as_str() {
        *app.ai_mode.lock().unwrap() = match mode {
            "manual" => "manual".into(),
            "plan" => "plan".into(),
            _ => "auto".into(),
        };
    }
    let attachments = p["attachments"].as_array().cloned().unwrap_or_default();
    let mut stored = Vec::new();
    // References to files already in the archive (no re-upload).
    for r in p["refs"].as_array().cloned().unwrap_or_default() {
        if let Some(name) = r.as_str() {
            let exists = {
                let eng = app.engine.lock().unwrap();
                store::sqlar_read(&eng.db, name)?.is_some()
            };
            if !exists {
                bail!("no such file in archive: {name}");
            }
            stored.push(json!({"name": name, "ref": true}));
        }
    }
    for att in &attachments {
        let name = att["name"].as_str().unwrap_or("upload.bin");
        let safe: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || ".-_ ".contains(c) { c } else { '_' })
            .collect();
        // Chat uploads are user content, so they land in the data/ tree.
        let path = format!("{}{}", store::DATA_DIR, safe.trim());
        let b64 = att["b64"].as_str().ok_or_else(|| anyhow!("attachment missing b64"))?;
        if b64.len() > MAX_UPLOAD_B64 {
            bail!("attachment too large ({}MB limit)", MAX_UPLOAD_BYTES / 1024 / 1024);
        }
        local_op(app, "file_put", json!({"name": path, "data": b64}))?;
        stored.push(json!({"name": path, "bytes": b64.len() * 3 / 4}));
    }
    let mid = format!("{}-{}", crate::store::now_ms(), fastrand::u32(..));
    let content = json!({"text": text, "attachments": stored});
    local_op(app, "chat",
        json!({"mid": mid, "role": "user", "content": content, "session": session}))?;
    spawn_ai(app.clone(), session.clone());
    Ok(json!({"ok": true, "mid": mid, "session": session}))
}

/// Start (or join) the assistant run for one chat session. Sessions run
/// independently: a second send on the SAME session is picked up by that
/// session's live loop, while another session starts its own thread.
pub fn spawn_ai(app: Arc<App>, session: String) {
    let stop = {
        let mut runs = app.ai_runs.lock().unwrap();
        if runs.contains_key(&session) {
            return; // this session already has a loop; it re-reads the history
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        runs.insert(
            session.clone(),
            crate::app::RunHandle { stop: stop.clone(), started: crate::store::now_ms() },
        );
        stop
    };
    app.notify("ai", json!({"state": "running", "session": session}));
    // wasm: single-threaded — ask the worker glue to call ai_tick(session) as
    // its next task, so the chat.send RPC that triggered this replies first.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = stop;
        crate::wasm::schedule_ai(&session);
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(move || {
        let ctx = crate::ai::RunCtx::root(session.clone(), stop);
        // A panic must not leave the run registered — that would silently
        // ignore every later chat.send on this session until a restart.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::ai::run(&app, &ctx)
        }))
        .unwrap_or_else(|_| Err(anyhow!("the assistant crashed (internal error) — please try again")));
        app.ai_runs.lock().unwrap().remove(&session);
        match result {
            Ok(_) => app.notify("ai", json!({"state": "idle", "session": session})),
            Err(e) => {
                app.notify("ai", json!({"state": "error", "message": e.to_string(),
                                        "session": session}));
                // Also surface it in chat so it syncs to everyone.
                let _ = local_op(
                    &app,
                    "chat",
                    json!({"mid": format!("err-{}-{}", crate::store::now_ms(), fastrand::u32(..)),
                           "role": "system", "session": session,
                           "content": {"text": format!("AI error: {e}")}}),
                );
            }
        }
    });
}

/// Session id from a request (`{session?}`), defaulting to the implicit main.
pub fn session_arg(p: &Value) -> Result<String> {
    match p["session"].as_str() {
        Some(s) => session_id(s),
        None => Ok("main".to_string()),
    }
}

/// Validate a session id: it names rows in stored state, so keep it to the
/// shapes we mint ('main', s-*, ag-*) plus anything plainly safe.
pub fn session_id(s: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 64
        || !s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!("bad session id (1-64 chars, letters/digits/-/_)");
    }
    Ok(s.to_string())
}

/// Every chat session: rows from uapp_sessions, plus the implicit 'main' and
/// any session that only exists as chat rows (e.g. replayed from an older
/// device). Newest-active first, archived ones last.
pub fn sessions_list(app: &Arc<App>) -> Result<Value> {
    let (metas, stats) = {
        let eng = app.engine.lock().unwrap();
        (
            store::query(
                &eng.db,
                "SELECT id, title, kind, parent, archived, created FROM uapp_sessions",
                &[],
            )?,
            store::query(
                &eng.db,
                "SELECT COALESCE(session,'main') AS s, count(*), max(ts) FROM uapp_chat
                 GROUP BY 1",
                &[],
            )?,
        )
    };
    let mut counts: std::collections::HashMap<String, (i64, i64)> = Default::default();
    for row in stats["rows"].as_array().cloned().unwrap_or_default() {
        counts.insert(
            row[0].as_str().unwrap_or("main").to_string(),
            (row[1].as_i64().unwrap_or(0), row[2].as_i64().unwrap_or(0)),
        );
    }
    let mut out: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let push = |id: String, title: Option<String>, kind: &str, parent: Value,
                    archived: bool, created: i64, out: &mut Vec<Value>| {
        let (messages, last_ts) = counts.get(&id).copied().unwrap_or((0, 0));
        out.push(json!({"id": id, "title": title, "kind": kind, "parent": parent,
                        "archived": archived, "created": created,
                        "messages": messages, "last_ts": last_ts}));
    };
    for row in metas["rows"].as_array().cloned().unwrap_or_default() {
        let id = row[0].as_str().unwrap_or("").to_string();
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        push(
            id,
            row[1].as_str().map(|s| s.to_string()),
            row[2].as_str().unwrap_or("chat"),
            row[3].clone(),
            row[4].as_i64().unwrap_or(0) != 0,
            row[5].as_i64().unwrap_or(0),
            &mut out,
        );
    }
    // 'main' needs no row, and neither does a session whose metadata op hasn't
    // arrived yet — surface them from the chat rows themselves.
    let mut implicit: Vec<String> = counts.keys().cloned().collect();
    implicit.push("main".to_string());
    implicit.sort();
    for id in implicit {
        if !seen.insert(id.clone()) {
            continue;
        }
        let title = if id == "main" { Some("Main chat".to_string()) } else { None };
        let kind = if id.starts_with("ag-") { "agent" } else { "chat" };
        let created = counts.get(&id).map(|(_, t)| *t).unwrap_or(0);
        push(id, title, kind, Value::Null, false, created, &mut out);
    }
    out.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["archived"].as_bool().unwrap_or(false),
                -(v["last_ts"].as_i64().unwrap_or(0).max(v["created"].as_i64().unwrap_or(0))),
                v["id"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
    Ok(json!(out))
}

// ---- web: search + page fetch ----------------------------------------------

fn strip_html(html: &str) -> (String, String) {
    // title
    let title = html
        .split("<title")
        .nth(1)
        .and_then(|s| s.split_once('>'))
        .and_then(|(_, rest)| rest.split("</title>").next())
        .unwrap_or("")
        .trim()
        .to_string();
    // drop script/style/head blocks, then tags, then decode common entities
    let mut s = html.to_string();
    for tag in ["script", "style", "head", "noscript", "svg", "template"] {
        loop {
            // ASCII lowering keeps byte offsets valid in `s`; full Unicode
            // lowercasing can change the length and panic on hostile HTML.
            let lower = s.to_ascii_lowercase();
            let Some(start) = lower.find(&format!("<{tag}")) else { break };
            let Some(end) = lower[start..].find(&format!("</{tag}>")) else {
                s.truncate(start);
                break;
            };
            s.replace_range(start..start + end + tag.len() + 3, " ");
        }
    }
    let mut out = String::with_capacity(s.len() / 4);
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'");
    let mut collapsed = String::with_capacity(out.len());
    let mut last_ws = 0u8; // 0=text, 1=space, 2=newline
    for c in out.chars() {
        if c == '\n' {
            if last_ws < 2 {
                collapsed.push('\n');
            }
            last_ws = 2;
        } else if c.is_whitespace() {
            if last_ws == 0 {
                collapsed.push(' ');
            }
            if last_ws < 1 {
                last_ws = 1;
            }
        } else {
            collapsed.push(c);
            last_ws = 0;
        }
    }
    (title, collapsed.trim().to_string())
}

/// Truncate to at most `max` bytes without panicking mid-char.
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Best-effort SSRF guard for model/app-triggered fetches: refuse URLs whose
/// host resolves to loopback, private, link-local (incl. cloud metadata) or
/// unspecified addresses. UAPP_ALLOW_LOCAL_FETCH=1 opts out (tests, intranet
/// setups).
pub(crate) fn check_public_url(url: &str) -> Result<()> {
    // Browser build: there is no OS network access at all — the browser's own
    // same-origin/CORS model gates every request, and DNS is unavailable to
    // resolve-and-check. Let the fetch itself succeed or fail.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = url;
        return Ok(());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
    if std::env::var("UAPP_ALLOW_LOCAL_FETCH").map(|v| v == "1").unwrap_or(false) {
        return Ok(());
    }
    let host = url
        .split("//")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .map(|a| a.rsplit('@').next().unwrap_or(a))
        .map(|a| {
            if let Some(h) = a.strip_prefix('[') {
                h.split(']').next().unwrap_or(h).to_string()
            } else {
                a.split(':').next().unwrap_or(a).to_string()
            }
        })
        .unwrap_or_default();
    if host.is_empty() {
        bail!("could not parse the URL host");
    }
    fn is_local(ip: std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                v4.is_loopback() || v4.is_private() || v4.is_link_local()
                    || v4.is_unspecified() || v4.is_broadcast()
                    || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // ULA
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local
                    || v6.to_ipv4_mapped().map_or(false, |v4| is_local(std::net::IpAddr::V4(v4)))
            }
        }
    }
    let addrs: Vec<std::net::IpAddr> = if let Ok(ip) = host.parse() {
        vec![ip]
    } else {
        use std::net::ToSocketAddrs;
        (host.as_str(), 80)
            .to_socket_addrs()
            .map(|it| it.map(|a| a.ip()).collect())
            .unwrap_or_default() // unresolvable: let the fetch report the error
    };
    if addrs.iter().any(|ip| is_local(*ip)) {
        bail!("refusing to fetch {host}: it resolves to a local/private address");
    }
    Ok(())
    }
}

fn url_decode(s: &str) -> String {
    let b = s.replace('+', " ");
    let bytes = b.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&b[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// Web search via DuckDuckGo's HTML endpoint (no API key needed).
/// UAPP_SEARCH_URL overrides the endpoint template ({} = query) for tests.
fn web_search(p: Value) -> Result<Value> {
    let query = need_str(&p, "query")?;
    let count = p["count"].as_u64().unwrap_or(6).min(10) as usize;
    let template = std::env::var("UAPP_SEARCH_URL")
        .unwrap_or_else(|_| "https://html.duckduckgo.com/html/?q={}".into());
    let url = template.replace("{}", &url_encode(query));
    let resp = crate::net::request("GET", &url, &[], None, 15, 45)
        .map_err(|e| anyhow!("search request failed: {e}"))?;
    if resp.status >= 400 {
        bail!("search request failed: HTTP {}", resp.status);
    }
    let html = resp.into_string()?;
    let mut results = Vec::new();
    for chunk in html.split("result__a").skip(1) {
        if results.len() >= count {
            break;
        }
        let Some(href) = chunk.split("href=\"").nth(1).and_then(|s| s.split('"').next()) else {
            continue;
        };
        // DDG wraps targets: //duckduckgo.com/l/?uddg=<encoded>&...
        let target = if let Some(enc) = href.split("uddg=").nth(1) {
            url_decode(enc.split('&').next().unwrap_or(enc))
        } else if href.starts_with("http") {
            href.to_string()
        } else {
            continue;
        };
        let title = chunk
            .split_once('>')
            .map(|(_, rest)| strip_html(rest.split("</a>").next().unwrap_or("")).1)
            .unwrap_or_default();
        let snippet = chunk
            .split("result__snippet")
            .nth(1)
            .and_then(|s| s.split_once('>'))
            .map(|(_, rest)| strip_html(rest.split("</a>").next().unwrap_or("")).1)
            .unwrap_or_default();
        if !title.is_empty() {
            results.push(json!({"title": title, "url": target, "snippet": snippet}));
        }
    }
    if results.is_empty() {
        bail!("no results parsed (search engine may be rate-limiting; try again or fetch_url a known site)");
    }
    Ok(json!({"query": query, "results": results}))
}

/// Fetch a web page and return readable text (for the model or the app).
fn web_fetch(p: Value) -> Result<Value> {
    let url = need_str(&p, "url")?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("fetch_url only supports http(s) URLs");
    }
    if p["_user_approved"] != json!(true) {
        check_public_url(url)?;
    }
    let resp = crate::net::request("GET", url, &[], None, 15, 45)
        .map_err(|e| anyhow!("fetch failed: {e}"))?;
    if resp.status >= 400 {
        bail!("HTTP {} from {url}", resp.status);
    }
    let ctype = resp.content_type.clone();
    let raw = resp.into_bytes_capped(4 * 1024 * 1024)?;
    let body = String::from_utf8_lossy(&raw).into_owned();
    let (title, text) = if ctype.contains("html") {
        strip_html(&body)
    } else {
        (String::new(), body)
    };
    let mut text = text;
    let truncated = text.len() > 20_000;
    if truncated {
        truncate_utf8(&mut text, 20_000);
        text.push_str("\n…(truncated)");
    }
    Ok(json!({"url": url, "title": title, "contentType": ctype,
              "text": text, "truncated": truncated}))
}

// ---- MCP server management --------------------------------------------------

fn mcp_mutate(app: &Arc<App>, f: impl FnOnce(&mut Vec<Value>)) -> Result<Value> {
    let mut servers = mcp_servers(app);
    f(&mut servers);
    local_op(app, "config_set", json!({"key": "mcp", "value": servers}))?;
    Ok(json!({"ok": true}))
}

fn mcp_add(app: &Arc<App>, p: Value) -> Result<Value> {
    let name: String = need_str(&p, "name")?
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if name.is_empty() {
        bail!("server name must be alphanumeric");
    }
    let url = need_str(&p, "url")?.to_string();
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("MCP server URL must be http(s)");
    }
    let mut entry = json!({"name": name, "url": url, "enabled": true});
    if let Some(tok) = p["bearer_token"].as_str() {
        if !tok.is_empty() {
            entry["headers"] = json!({"Authorization": format!("Bearer {tok}")});
        }
    }
    if p["headers"].is_object() {
        entry["headers"] = p["headers"].clone();
    }
    // Verify it answers before saving (clear error for the model/user).
    let tools = crate::mcp::list_all_tools(&[entry.clone()]);
    if tools.is_empty() {
        bail!("could not list tools from {url} — check the URL/token (server not saved)");
    }
    let n = tools.len();
    mcp_mutate(app, |servers| {
        servers.retain(|s| s["name"].as_str() != Some(&name));
        servers.push(entry);
    })?;
    Ok(json!({"ok": true, "name": name, "toolCount": n,
              "tools": tools.iter().map(|t| t["name"].clone()).collect::<Vec<_>>()}))
}

fn mcp_status(app: &Arc<App>) -> Value {
    let servers = mcp_servers(app);
    let mut out = Vec::new();
    for s in &servers {
        let name = s["name"].as_str().unwrap_or("?").to_string();
        let enabled = s["enabled"].as_bool().unwrap_or(true);
        let mut entry = json!({"name": name, "url": s["url"], "enabled": enabled,
                               "hasAuth": s["headers"].is_object()});
        if enabled {
            let tools = crate::mcp::list_all_tools(std::slice::from_ref(s));
            if tools.is_empty() {
                entry["error"] = json!("unreachable or no tools");
            } else {
                entry["toolCount"] = json!(tools.len());
                entry["tools"] = json!(tools.iter().map(|t| t["name"].clone()).collect::<Vec<_>>());
            }
        }
        out.push(entry);
    }
    json!(out)
}

// ---- vendoring: download a library/asset into the archive ------------------

const FETCH_LIMIT: usize = 25 * 1024 * 1024;

/// Biggest file that can be put into the archive by hand (upload, drag-drop,
/// chat attachment). base64 inflates by 4/3, and the WebSocket frame limit in
/// `server.rs` is sized to hold that.
pub const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_UPLOAD_B64: usize = MAX_UPLOAD_BYTES / 3 * 4 + 64;

/// Download a URL server-side and store it in the archive as ONE
/// op — the library then ships inside the .uapp to every user, works offline,
/// and is served same-origin (no CORS; .wasm gets application/wasm so
/// WebAssembly.instantiateStreaming works).
fn files_fetch(app: &Arc<App>, p: Value) -> Result<Value> {
    let url = need_str(&p, "url")?.to_string();
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("files.fetch only supports http(s) URLs");
    }
    if p["_user_approved"] != json!(true) {
        check_public_url(&url)?;
    }
    let name = match p["name"].as_str() {
        Some(n) => n.to_string(),
        None => {
            let base = url
                .split('/')
                .last()
                .unwrap_or("lib")
                .split(['?', '#'])
                .next()
                .unwrap_or("lib");
            format!("vendor/{}", if base.is_empty() { "lib" } else { base })
        }
    };
    if name.contains("..") {
        bail!("bad target name");
    }
    let resp = crate::net::request("GET", &url, &[], None, 20, 120)
        .map_err(|e| anyhow!("download failed: {e}"))?;
    if resp.status >= 400 {
        bail!("download failed: HTTP {} from {url}", resp.status);
    }
    let ctype = resp.content_type.clone();
    let data = resp.into_bytes_capped((FETCH_LIMIT + 1) as u64)?;
    if data.len() > FETCH_LIMIT {
        bail!("file exceeds the {}MB vendoring limit", FETCH_LIMIT / 1024 / 1024);
    }
    if data.is_empty() {
        bail!("downloaded 0 bytes from {url}");
    }
    let name = store::canon_name(&name);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    local_op(app, "file_put", json!({"name": name, "data": b64}))?;
    Ok(json!({"name": name, "bytes": data.len(), "contentType": ctype, "url": url}))
}

// ---- native file drops ----------------------------------------------------

/// A drop id is good for two minutes — long enough for the user to answer an
/// "App or Data?" prompt, short enough that nothing lingers.
const DROP_TTL_MS: u64 = 120_000;

/// Claim the paths a native drop announced, dropping the id so one drop can't
/// be replayed. Directories are reported back as skipped rather than walked.
fn take_drop(app: &Arc<App>, p: &Value) -> Result<(Vec<std::path::PathBuf>, Vec<String>)> {
    let id = need_str(p, "id")?.to_string();
    let now = crate::store::now_ms();
    let paths = {
        let mut drops = app.drops.lock().unwrap();
        drops.retain(|_, (ts, _)| now.saturating_sub(*ts) < DROP_TTL_MS);
        match drops.remove(&id) {
            Some((_, paths)) => paths,
            None => bail!("that drop has expired — drag the files in again"),
        }
    };
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        if path.is_dir() {
            skipped.push(format!("{label} (folders can't be dropped in yet)"));
        } else if !path.is_file() {
            skipped.push(format!("{label} (not a file)"));
        } else {
            files.push(path);
        }
    }
    Ok((files, skipped))
}

fn read_dropped(path: &std::path::Path) -> Result<Vec<u8>> {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len > MAX_UPLOAD_BYTES as u64 {
        bail!("over the {}MB limit", MAX_UPLOAD_BYTES / 1024 / 1024);
    }
    Ok(std::fs::read(path)?)
}

/// Store dropped files in the archive under `dir` (an archive folder, e.g.
/// "data/"). This is the native window's equivalent of the browser's upload.
fn drop_ingest(app: &Arc<App>, p: Value) -> Result<Value> {
    let dir = p["dir"].as_str().unwrap_or(store::DATA_DIR).trim().to_string();
    if dir.contains("..") {
        bail!("bad target folder");
    }
    let dir = if dir.is_empty() || dir.ends_with('/') { dir } else { format!("{dir}/") };
    let (files, mut skipped) = take_drop(app, &p)?;
    let mut stored = Vec::new();
    for path in files {
        let base = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let name = store::canon_name(&format!("{dir}{base}"));
        match read_dropped(&path) {
            Ok(data) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                local_op(app, "file_put", json!({"name": name, "data": b64}))?;
                stored.push(json!({"name": name, "bytes": data.len()}));
            }
            Err(e) => skipped.push(format!("{base} ({e})")),
        }
    }
    Ok(json!({"ok": true, "files": stored, "skipped": skipped}))
}

// ---- update from a template -----------------------------------------------
// A .uapp dropped on the window is almost never meant to become a file inside
// the app: it's a newer version of the app itself. So the shell routes it here
// instead of to `drop.ingest`, the user is shown what would change, and only
// then is it applied. See `crate::template` for what "update" means precisely.

/// A staged template expires as soon as it stops being plausible that the
/// dialog is still on screen.
const TEMPLATE_TTL_MS: u64 = 15 * 60 * 1000;

/// The dropped/uploaded bytes plus the file name they arrived as. Either the
/// page sends them (browser upload / the wasm demo) or it names a native drop
/// id, in which case the bytes are read server-side and the page never learns
/// the path — the same rule as `drop.ingest`.
fn template_bytes(app: &Arc<App>, p: &Value) -> Result<(Vec<u8>, String)> {
    if let Some(b64) = p["b64"].as_str() {
        if b64.len() > MAX_UPLOAD_B64 {
            bail!("that file is over the {}MB limit", MAX_UPLOAD_BYTES / 1024 / 1024);
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
        let label = p["name"].as_str().unwrap_or("template.uapp").to_string();
        return Ok((bytes, label));
    }
    if p["id"].is_string() {
        let (files, _) = take_drop(app, p)?;
        let [path] = &files[..] else {
            bail!("drop a single .uapp file to update this app from it");
        };
        let label = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        return Ok((read_dropped(path)?, label));
    }
    bail!("template.inspect needs the file (b64) or a native drop id");
}

/// Stage a template and report what updating from it would change. Read-only:
/// nothing is written until `template.apply` runs with the token.
fn template_inspect(app: &Arc<App>, p: Value) -> Result<Value> {
    let password = p["password"].as_str().filter(|s| !s.is_empty()).map(String::from);
    let (bytes, label) = template_bytes(app, &p)?;
    let plan = {
        let src = crate::template::Source::from_bytes(&bytes, password.as_deref())?;
        let eng = app.engine.lock().unwrap();
        let mut plan = crate::template::plan(&eng.db, src.conn())?;
        // What only this layer knows: the name it was dropped as, and whether
        // it came out of this very app (so the dialog can say so).
        plan["source"]["file"] = json!(label);
        plan["source"]["bytes"] = json!(bytes.len());
        plan["source"]["sameApp"] =
            json!(plan["source"]["appId"].as_str() == Some(eng.app_id.as_str()));
        plan
    };
    let token: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
    {
        let mut staged = app.templates.lock().unwrap();
        let now = store::now_ms();
        staged.retain(|_, s| now.saturating_sub(s.ts) < TEMPLATE_TTL_MS);
        // A template can be tens of megabytes; never hold more than the one
        // the user is looking at plus the one they just abandoned.
        if staged.len() > 1 {
            staged.clear();
        }
        staged.insert(
            token.clone(),
            crate::app::StagedTemplate { ts: now, label, bytes, password },
        );
    }
    Ok(json!({"token": token, "plan": plan}))
}

/// Commit a staged update. One op, so the whole thing is one transaction: app
/// files, schema and config all land together or not at all.
fn template_apply(app: &Arc<App>, p: Value) -> Result<Value> {
    let token = need_str(&p, "token")?;
    let remove_stale = p["remove_stale"].as_bool().unwrap_or(true);
    let staged = app
        .templates
        .lock()
        .unwrap()
        .remove(token)
        .ok_or_else(|| anyhow!("that template is no longer staged — drop it in again"))?;
    // Rescue point before a whole-app change, like an encryption switch.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut eng = app.engine.lock().unwrap();
        if let Err(e) = eng.snapshot() {
            eprintln!("uapp: snapshot before the template update failed: {e:#}");
        }
    }
    let mut payload = json!({
        "b64": base64::engine::general_purpose::STANDARD.encode(&staged.bytes),
        "remove_stale": remove_stale,
    });
    if let Some(pw) = &staged.password {
        payload["password"] = json!(pw);
    }
    let mut out = local_op(app, "template_update", payload)?;
    if let Some(o) = out.as_object_mut() {
        o.insert("file".into(), json!(staged.label));
    }
    Ok(out)
}

// ---- rename / move --------------------------------------------------------

/// Canonical form of a folder reference: always ends in "/", and the two roots
/// stay themselves (`canon_name("app")` would otherwise nest it).
fn canon_dir(name: &str) -> String {
    let n = name.trim().trim_start_matches('/').trim_end_matches('/');
    match n {
        "" => String::new(),
        "app" | "data" => format!("{n}/"),
        "uploads" => store::DATA_DIR.to_string(), // legacy upload folder
        _ => format!("{}/", store::canon_name(n)),
    }
}

fn check_target(to: &str) -> Result<()> {
    if to.is_empty() || to.len() > 512 || to.contains("..") {
        bail!("bad target name (must be 1-512 chars and contain no '..')");
    }
    Ok(())
}

/// Rename one file, or — when `from` ends in "/" — a whole folder and
/// everything under it. sqlar has no rename, so each file is copied to the new
/// name and the old one dropped: two ops per file, which is also what keeps
/// every device's file history intact. Moving between `app/` and `data/` is a
/// plain rename, and that is how a file changes role.
fn files_rename(app: &Arc<App>, p: Value) -> Result<Value> {
    let from_raw = need_str(&p, "from")?.to_string();
    let to_raw = need_str(&p, "to")?.trim().to_string();
    check_target(&to_raw)?;
    if !from_raw.ends_with('/') {
        let from = {
            let eng = app.engine.lock().unwrap();
            store::resolve_name(&eng.db, &from_raw)?
                .unwrap_or_else(|| store::canon_name(&from_raw))
        };
        let to = store::canon_name(&to_raw);
        if to.ends_with('/') {
            bail!("a file name can't end with '/'");
        }
        if to == from {
            return Ok(json!({"ok": true, "from": from, "to": to}));
        }
        let (data, taken) = {
            let eng = app.engine.lock().unwrap();
            (
                store::sqlar_read(&eng.db, &from)?,
                store::sqlar_read(&eng.db, &to)?.is_some(),
            )
        };
        let Some(data) = data else { bail!("no such file: {from}") };
        if taken {
            bail!("a file named {to} already exists");
        }
        local_op(app, "file_put", json!({"name": to,
            "data": base64::engine::general_purpose::STANDARD.encode(&data)}))?;
        local_op(app, "file_del", json!({"name": from}))?;
        return Ok(json!({"ok": true, "from": from, "to": to}));
    }

    let from = canon_dir(&from_raw);
    let to = canon_dir(&to_raw);
    if to.is_empty() {
        bail!("a folder needs a name");
    }
    if from == store::APP_DIR || from == store::DATA_DIR {
        bail!("App and Data are the app's two top-level folders — they can't be renamed. Rename or move what's inside them instead.");
    }
    if to == store::APP_DIR || to == store::DATA_DIR {
        bail!("that would replace a top-level folder — give the folder a name inside App or Data");
    }
    if to == from {
        return Ok(json!({"ok": true, "from": from, "to": to, "moved": 0}));
    }
    if to.starts_with(&from) {
        bail!("can't move a folder into itself");
    }
    // Every file under the prefix, with its destination name.
    let moves: Vec<(String, String)> = {
        let eng = app.engine.lock().unwrap();
        let mut stmt = eng
            .db
            .prepare("SELECT name FROM sqlar WHERE name LIKE ?1 || '%' ORDER BY name")?;
        let rows = stmt.query_map(rusqlite::params![&from], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let old = row?;
            out.push((old.clone(), format!("{to}{}", &old[from.len()..])));
        }
        out
    };
    if moves.is_empty() {
        bail!("no such folder: {from}");
    }
    {
        let eng = app.engine.lock().unwrap();
        let mut stmt = eng.db.prepare("SELECT 1 FROM sqlar WHERE name=?1")?;
        for (_, new) in &moves {
            if stmt.exists(rusqlite::params![new])? {
                bail!("{new} already exists — rename or remove it first");
            }
        }
    }
    for (old, new) in &moves {
        let data = {
            let eng = app.engine.lock().unwrap();
            store::sqlar_read(&eng.db, old)?
        };
        let Some(data) = data else { continue }; // vanished mid-move
        local_op(app, "file_put", json!({"name": new,
            "data": base64::engine::general_purpose::STANDARD.encode(&data)}))?;
        local_op(app, "file_del", json!({"name": old}))?;
    }
    Ok(json!({"ok": true, "from": from, "to": to, "moved": moves.len()}))
}

// ---- CSV import -----------------------------------------------------------

fn parse_csv(text: &str, delim: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.is_empty() {
            in_quotes = true;
        } else if c == delim {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut field));
            if !(row.len() == 1 && row[0].is_empty()) {
                rows.push(std::mem::take(&mut row));
            } else {
                row.clear();
            }
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn infer(v: &str) -> Value {
    let t = v.trim();
    if t.is_empty() {
        return Value::Null;
    }
    if let Ok(i) = t.parse::<i64>() {
        // Preserve leading zeros (invoice numbers etc.) as text.
        if !(t.len() > 1 && t.starts_with('0')) {
            return json!(i);
        }
    }
    if let Ok(f) = t.parse::<f64>() {
        if t.chars().any(|c| c == '.' || c == 'e' || c == 'E') {
            return json!(f);
        }
    }
    json!(v)
}

fn ident(s: &str) -> String {
    let mut out: String = s
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() || out.chars().next().unwrap().is_numeric() {
        format!("c_{out}")
    } else {
        out
    }
}

/// Parse a CSV already stored in the archive (e.g. dragged into chat) and
/// turn it into ONE import op — complex queries then run against a real
/// SQLite table, and the log carries a single compressed op.
pub fn import_csv(app: &Arc<App>, p: Value) -> Result<Value> {
    let file = need_str(&p, "file")?;
    let table = ident(p["table"].as_str().unwrap_or(
        file.rsplit('/').next().unwrap_or("data").trim_end_matches(".csv"),
    ));
    let data = {
        let eng = app.engine.lock().unwrap();
        store::sqlar_read(&eng.db, file)?.ok_or_else(|| anyhow!("no such file: {file}"))?
    };
    let text = String::from_utf8_lossy(&data).to_string();
    let first_line = text.lines().next().unwrap_or("");
    let delim = p["delimiter"].as_str().and_then(|s| s.chars().next()).unwrap_or_else(|| {
        [',', ';', '\t', '|']
            .into_iter()
            .max_by_key(|d| first_line.matches(*d).count())
            .unwrap_or(',')
    });
    let mut rows = parse_csv(&text, delim);
    if rows.is_empty() {
        bail!("no rows found in {file}");
    }
    let header = p["header"].as_bool().unwrap_or(true);
    let columns: Vec<String> = if header {
        let h = rows.remove(0);
        let mut seen = std::collections::HashSet::new();
        h.iter()
            .enumerate()
            .map(|(i, c)| {
                let mut name = ident(c);
                if name.is_empty() {
                    name = format!("c{i}");
                }
                while !seen.insert(name.clone()) {
                    name.push('_');
                }
                name
            })
            .collect()
    } else {
        (0..rows[0].len()).map(|i| format!("c{i}")).collect()
    };
    let ncols = columns.len();
    let json_rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut vals: Vec<Value> = r.iter().take(ncols).map(|v| infer(v)).collect();
            while vals.len() < ncols {
                vals.push(Value::Null);
            }
            Value::Array(vals)
        })
        .collect();
    let create = format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" ({});",
        table,
        columns
            .iter()
            .map(|c| format!("\"{}\"", c))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let n = json_rows.len();
    local_op(
        app,
        "import",
        json!({
            "table": table,
            "create": create,
            "truncate": p["replace"].as_bool().unwrap_or(false),
            "columns": columns,
            "rows": json_rows,
        }),
    )?;
    Ok(json!({"table": table, "rows": n, "columns": columns}))
}
