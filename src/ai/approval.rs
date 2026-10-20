//! Blocking on the user: approval prompts for gated tools and ask_user.
//!
//! Natively the run thread waits on a channel the shell's answer RPC fills.
//! In the browser build the worker is blocked inside the run, so it can't
//! receive that RPC; it asks over the SharedArrayBuffer bridge instead and
//! the main thread shows the prompt and writes the decision back. Both paths
//! go through [`prompt_approval`] and [`prompt_questions`].

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::store;

use super::{current_ctx, RunCtx};

/// One approval prompt.
struct ApprovalReq<'a> {
    id: &'a str,
    /// The tool name shown on the prompt.
    name: &'a str,
    /// The chat the prompt belongs to ("app" for app-triggered calls).
    session: &'a str,
    input: &'a Value,
    note: Option<String>,
    /// What the browser build sends over the bridge: its kind and payload.
    bridge: (&'static str, Value),
}

/// Show an approval prompt and block until the user decides. Returns
/// `(allow, always)`; a timeout, a stop or an unreachable shell all come
/// back as a denial.
fn prompt_approval(app: &Arc<App>, req: ApprovalReq) -> (bool, bool) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, req.id, req.name, req.session, req.input, &req.note);
        match crate::wasm::bridge_call(req.bridge.0, &req.bridge.1) {
            Ok(v) => {
                let allow = v["allow"].as_bool().unwrap_or(false);
                (allow, allow && v["always"].as_bool().unwrap_or(false))
            }
            Err(_) => (false, false),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = &req.bridge;
        let (tx, rx) = std::sync::mpsc::sync_channel::<(bool, bool)>(1);
        crate::app::lock(&app.pending).insert(
            req.id.to_string(),
            crate::app::PendingApproval {
                name: req.name.to_string(),
                session: req.session.to_string(),
                input: req.input.clone(),
                created: crate::store::now_ms(),
                note: req.note,
                tx,
            },
        );
        app.notify_pending();
        let decision = rx.recv_timeout(std::time::Duration::from_secs(300));
        crate::app::lock(&app.pending).remove(req.id);
        app.notify_pending();
        match decision {
            Ok((allow, always)) => (allow, allow && always),
            Err(_) => (false, false), // timeout or channel dropped => deny
        }
    }
}

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
    let note = forced.as_ref().map(|(_, reason)| reason.clone());
    let (allow, always) = prompt_approval(
        app,
        ApprovalReq {
            id,
            name,
            // The RUN's session (not a sub-agent's), so ai.stop on the chat the
            // user is looking at resolves prompts raised deep inside it.
            session: &ctx.root,
            input,
            bridge: (
                "approval",
                json!({"id": id, "name": name, "input": input,
                       "note": note, "session": ctx.root}),
            ),
            note,
        },
    );
    if always {
        let key = forced
            .as_ref()
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| name.to_string());
        crate::app::lock(&app.always_allow).insert(key);
    }
    allow
}

/// Approval gate for app-triggered tool calls. Unlike AI-initiated calls,
/// app-triggered calls always prompt (as if in manual mode) unless the tool
/// has been "always allowed" for this app. Returns true=run it, false=denied.
pub fn app_approval_gate(app: &Arc<App>, tool: &str, input: &Value) -> Result<bool> {
    let app_id = app.eng().app_id.clone();

    // Check if this tool has been pre-approved for this app
    if crate::secrets::app_approved(app, &app_id, tool) {
        return Ok(true);
    }

    let id = format!("app-{}-{}", tool, fastrand::u32(..));
    let app_name = {
        let eng = app.eng();
        store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "uapp".into())
    };
    let (allow, always) = prompt_approval(
        app,
        ApprovalReq {
            id: &id,
            name: tool,
            session: "app", // app-triggered, not tied to any chat session
            input,
            note: Some(format!("'{}' is requesting access", app_name)),
            bridge: (
                "approval.app",
                json!({"id": id, "app": app_name, "appId": app_id,
                       "tool": tool, "input": input}),
            ),
        },
    );
    if always {
        crate::secrets::approve_app(app, &app_id, tool)?;
    }
    Ok(allow)
}

/// What ask_user returns when nobody answered.
fn no_answer(note: &str) -> Value {
    json!({"answered": false, "note": note})
}

const NO_ANSWER: &str = "no answer within the wait window — proceed on your best judgment \
                         and say what you assumed";

/// Normalize ask_user's questions so the UI can render without defensive code.
fn normalize_questions(input: &Value) -> Result<Vec<Value>> {
    let raw = input["questions"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow!("ask_user needs a non-empty questions array"))?;
    if raw.len() > 4 {
        bail!("ask_user takes at most 4 questions per call");
    }
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
    Ok(questions)
}

/// Pair each answer with its question text so the transcript reads on its
/// own; unanswered questions come back with empty fields.
fn shape_answers(questions: &[Value], reply: &Value) -> Value {
    let empty = Vec::new();
    let got = reply["answers"].as_array().unwrap_or(&empty);
    let mut answers = Vec::new();
    let mut any = false;
    for (i, q) in questions.iter().enumerate() {
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
        return no_answer("the user submitted an empty answer");
    }
    json!({"answered": true, "answers": answers})
}

/// Show the questions and block until the user answers. Returns the raw
/// reply (`{"answers": [...]}` or `{"stopped": true}`), or None when nobody
/// answered in time.
fn prompt_questions(app: &Arc<App>, id: &str, questions: &Value, session: &str) -> Option<Value> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        crate::wasm::bridge_call(
            "ask_user",
            &json!({"id": id, "questions": questions, "session": session}),
        )
        .ok()
        .filter(|v| v["answers"].is_array())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Value>(1);
        crate::app::lock(&app.questions).insert(
            id.to_string(),
            crate::app::PendingQuestion {
                questions: questions.clone(),
                session: session.to_string(),
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
        crate::app::lock(&app.questions).remove(id);
        app.notify_questions();
        answer.ok()
    }
}

/// The ask_user tool: block this run until the user answers 1-4 questions
/// (radio for single choice, checkboxes for multiSelect, always with a
/// free-form "Other" field). Timeout/stop resolve as "no answer" so the
/// model can proceed on its best judgment instead of erroring.
pub fn ask_user(app: &Arc<App>, input: &Value) -> Result<Value> {
    let ctx = current_ctx();
    let questions = normalize_questions(input)?;
    let id = format!("q-{}-{}", crate::store::now_ms(), fastrand::u32(..));
    match prompt_questions(app, &id, &json!(questions), &ctx.root) {
        Some(v) if v["stopped"] == json!(true) => {
            Ok(no_answer("the run was stopped before the user answered"))
        }
        Some(v) => Ok(shape_answers(&questions, &v)),
        None => Ok(no_answer(NO_ANSWER)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_are_normalized() {
        let q = normalize_questions(&json!({"questions": [
            {"question": "  Which? ", "header": "a very long header", "options": ["One", {"label": "Two", "description": "d"}, ""]}
        ]}))
        .unwrap();
        assert_eq!(q[0]["question"], "Which?");
        assert_eq!(q[0]["header"], "a very long ");
        assert_eq!(q[0]["options"].as_array().unwrap().len(), 2);
        assert!(normalize_questions(&json!({"questions": []})).is_err());
        assert!(normalize_questions(&json!({"questions": [{}, {}, {}, {}, {}]})).is_err());
    }

    #[test]
    fn answers_pair_with_questions() {
        let q = vec![json!({"question": "A?"}), json!({"question": "B?"})];
        let v = shape_answers(
            &q,
            &json!({"answers": [{"choices": ["x"]}, {"other": " y "}]}),
        );
        assert_eq!(v["answered"], true);
        assert_eq!(v["answers"][1]["other"], "y");
        let empty = shape_answers(&q, &json!({"answers": []}));
        assert_eq!(empty["answered"], false);
    }
}
