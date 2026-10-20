//! Blocking on the user: approval prompts for gated tools and ask_user.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::store;

use super::{current_ctx, RunCtx};

/// Gate a tool call behind user approval: in manual mode every gated tool
/// asks; calls with a `forced` reason (local database files, local network
/// hosts) ask in auto mode too. Returns true=run it, false=denied. Blocks the
/// AI thread until the user decides, the 5-minute timeout expires (deny), or
/// ai.stop fires.
pub(crate) fn approval_gate(
    app: &Arc<App>,
    ctx: &RunCtx,
    id: &str,
    name: &str,
    input: &Value,
    forced: &Option<(String, String)>,
) -> bool {
    match forced {
        None => {
            let manual = crate::app::lock(&app.ai_mode).as_str() == "manual";
            if !manual || !crate::tools::is_gated(app, name) {
                return true;
            }
            if crate::app::lock(&app.always_allow).contains(name) {
                return true;
            }
        }
        // Forced prompts honor their own, narrower always-allow key (e.g.
        // "fetch_url:local"), so approving OBS control once doesn't wave
        // through every future gated call of that tool.
        Some((key, _)) => {
            if crate::app::lock(&app.always_allow).contains(key) {
                return true;
            }
        }
    }
    // wasm: the worker is blocked inside this run, so the shell can't answer
    // an RPC prompt — ask over the SharedArrayBuffer bridge instead (the main
    // thread shows the prompt and writes the decision back).
    #[cfg(target_arch = "wasm32")]
    {
        let reply = crate::wasm::bridge_call(
            "approval",
            &json!({
                "id": id, "name": name, "input": input,
                "note": forced.as_ref().map(|(_, r)| r.clone()),
                "session": ctx.root,
            }),
        );
        return match reply {
            Ok(v) => {
                let allow = v["allow"].as_bool().unwrap_or(false);
                if allow && v["always"].as_bool().unwrap_or(false) {
                    let key = forced
                        .as_ref()
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| name.to_string());
                    crate::app::lock(&app.always_allow).insert(key);
                }
                allow
            }
            Err(_) => false,
        };
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(bool, bool)>(1);
        crate::app::lock(&app.pending).insert(
            id.to_string(),
            crate::app::PendingApproval {
                name: name.to_string(),
                // The RUN's session (not a sub-agent's), so ai.stop on the chat the
                // user is looking at resolves prompts raised deep inside it.
                session: ctx.root.clone(),
                input: input.clone(),
                created: crate::store::now_ms(),
                note: forced.as_ref().map(|(_, reason)| reason.clone()),
                tx,
            },
        );
        app.notify_pending();
        let decision = rx.recv_timeout(std::time::Duration::from_secs(300));
        crate::app::lock(&app.pending).remove(id);
        app.notify_pending();
        match decision {
            Ok((allow, always)) => {
                if allow && always {
                    let key = forced
                        .as_ref()
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| name.to_string());
                    crate::app::lock(&app.always_allow).insert(key);
                }
                allow
            }
            Err(_) => false, // timeout or channel dropped => deny
        }
    }
}

/// Approval gate for app-triggered tool calls. Unlike AI-initiated calls,
/// app-triggered calls always prompt (as if in manual mode) unless the tool
/// has been "always allowed" for this app. Returns true=run it, false=denied.
pub fn app_approval_gate(app: &Arc<App>, tool: &str, input: &Value) -> Result<bool> {
    let eng = app.eng();
    let app_id = eng.app_id.clone();
    drop(eng);

    // Check if this tool has been pre-approved for this app
    if crate::secrets::app_approved(app, &app_id, tool) {
        return Ok(true);
    }

    // Need to prompt for approval. Generate a unique ID for this request.
    let id = format!("app-{}-{}", tool, fastrand::u32(..));

    // Get the app name for the prompt
    let app_name = {
        let eng = app.eng();
        store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "uapp".into())
    };

    #[cfg(target_arch = "wasm32")]
    {
        // wasm: use the bridge to prompt
        let reply = crate::wasm::bridge_call(
            "approval.app",
            &json!({
                "id": id,
                "app": app_name,
                "appId": app_id,
                "tool": tool,
                "input": input,
            }),
        );
        match reply {
            Ok(v) => {
                let allow = v["allow"].as_bool().unwrap_or(false);
                if allow && v["always"].as_bool().unwrap_or(false) {
                    crate::secrets::approve_app(app, &app_id, tool)?;
                }
                Ok(allow)
            }
            Err(_) => Ok(false),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(bool, bool)>(1);
        crate::app::lock(&app.pending).insert(
            id.clone(),
            crate::app::PendingApproval {
                name: tool.to_string(),
                session: "app".to_string(), // app-triggered, not tied to any chat session
                input: input.clone(),
                created: crate::store::now_ms(),
                note: Some(format!("'{}' is requesting access", app_name)),
                tx,
            },
        );
        app.notify_pending();
        let decision = rx.recv_timeout(std::time::Duration::from_secs(300));
        crate::app::lock(&app.pending).remove(&id);
        app.notify_pending();
        match decision {
            Ok((allow, always)) => {
                if allow && always {
                    crate::secrets::approve_app(app, &app_id, tool)?;
                }
                Ok(allow)
            }
            Err(_) => Ok(false), // timeout => deny
        }
    }
}

/// The ask_user tool: block this run until the user answers 1-4 questions
/// (radio for single choice, checkboxes for multiSelect, always with a
/// free-form "Other" field). Timeout/stop resolve as "no answer" so the
/// model can proceed on its best judgment instead of erroring.
pub fn ask_user(app: &Arc<App>, input: &Value) -> Result<Value> {
    #[cfg(target_arch = "wasm32")]
    let _ = app;
    let ctx = current_ctx();
    let raw = input["questions"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow!("ask_user needs a non-empty questions array"))?;
    if raw.len() > 4 {
        bail!("ask_user takes at most 4 questions per call");
    }
    // Normalize each question so the UI can render without defensive code.
    let mut questions = Vec::new();
    for q in raw {
        let text = q["question"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("every entry needs a non-empty 'question'"))?;
        let header: String = q["header"]
            .as_str()
            .unwrap_or("")
            .trim()
            .chars()
            .take(12)
            .collect();
        let options: Vec<Value> = q["options"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| {
                        // Accept both {label, description} and bare strings.
                        let label = o["label"].as_str().or(o.as_str())?.trim();
                        if label.is_empty() {
                            return None;
                        }
                        let desc = o["description"].as_str().unwrap_or("").trim();
                        Some(json!({"label": label, "description": desc}))
                    })
                    .take(12)
                    .collect()
            })
            .unwrap_or_default();
        questions.push(json!({
            "question": text,
            "header": header,
            "options": options,
            "multiSelect": q["multiSelect"].as_bool().unwrap_or(false),
        }));
    }
    let questions = Value::Array(questions);
    let id = format!("q-{}-{}", crate::store::now_ms(), fastrand::u32(..));
    // wasm: prompt over the bridge (main thread renders it); an error or
    // unavailable bridge resolves as "no answer" so the model proceeds.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = &ctx;
        let reply = crate::wasm::bridge_call(
            "ask_user",
            &json!({
                "id": id, "questions": questions, "session": ctx.root,
            }),
        );
        let v = match reply {
            Ok(v) if v["answers"].is_array() => v,
            _ => {
                return Ok(json!({
                    "answered": false,
                    "note": "no answer within the wait window — proceed on your best judgment and say what you assumed"
                }))
            }
        };
        let empty = Vec::new();
        let got = v["answers"].as_array().unwrap_or(&empty);
        let mut answers = Vec::new();
        let mut any = false;
        for (i, q) in questions.as_array().unwrap().iter().enumerate() {
            let a = got.get(i).cloned().unwrap_or(json!({}));
            let choices: Vec<String> = a["choices"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let other = a["other"].as_str().map(str::trim).filter(|s| !s.is_empty());
            any = any || !choices.is_empty() || other.is_some();
            let mut row = json!({"question": q["question"], "choices": choices});
            if let Some(o) = other {
                row["other"] = json!(o);
            }
            answers.push(row);
        }
        if !any {
            return Ok(json!({"answered": false, "note": "the user submitted an empty answer"}));
        }
        return Ok(json!({"answered": true, "answers": answers}));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Value>(1);
        crate::app::lock(&app.questions).insert(
            id.clone(),
            crate::app::PendingQuestion {
                questions: questions.clone(),
                session: ctx.root.clone(),
                created: crate::store::now_ms(),
                tx,
            },
        );
        app.notify_questions();
        let timeout_ms: u64 = std::env::var("UAPP_ASK_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300_000);
        let answer = rx.recv_timeout(std::time::Duration::from_millis(timeout_ms));
        crate::app::lock(&app.questions).remove(&id);
        app.notify_questions();
        match answer {
            Ok(v) if v["stopped"] == json!(true) => Ok(json!({
                "answered": false,
                "note": "the run was stopped before the user answered"
            })),
            Ok(v) => {
                // Pair each answer with its question text so the transcript reads
                // on its own; unanswered questions come back with empty fields.
                let empty = Vec::new();
                let got = v["answers"].as_array().unwrap_or(&empty);
                let mut answers = Vec::new();
                let mut any = false;
                for (i, q) in questions.as_array().unwrap().iter().enumerate() {
                    let a = got.get(i).cloned().unwrap_or(json!({}));
                    let choices: Vec<String> = a["choices"]
                        .as_array()
                        .map(|c| {
                            c.iter()
                                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let other = a["other"].as_str().map(str::trim).filter(|s| !s.is_empty());
                    any = any || !choices.is_empty() || other.is_some();
                    let mut row = json!({"question": q["question"], "choices": choices});
                    if let Some(o) = other {
                        row["other"] = json!(o);
                    }
                    answers.push(row);
                }
                if !any {
                    return Ok(
                        json!({"answered": false, "note": "the user submitted an empty answer"}),
                    );
                }
                Ok(json!({"answered": true, "answers": answers}))
            }
            Err(_) => Ok(json!({
                "answered": false,
                "note": "no answer within the wait window — proceed on your best judgment \
                         and say what you assumed"
            })),
        }
    }
}
