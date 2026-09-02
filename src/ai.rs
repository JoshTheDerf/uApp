//! AI assistant: provider clients (Anthropic native; z.ai GLM and OpenRouter
//! via the OpenAI-compatible chat/completions shape) plus the tool loop.
//! Every tool call goes through the same JSON-RPC dispatch the user's shell
//! and the embedded app use — the model has exactly the user's powers.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::rpc;
use crate::app::App;
use crate::store;

const MAX_ITERATIONS: usize = 40;
const TOOL_RESULT_LIMIT: usize = 30_000;
/// Byte budget for tool_result content in the OUTGOING messages (newest-first).
/// Stored history is never touched — only what we send this turn.
const OUTGOING_TOOL_BUDGET: usize = 96 * 1024;
/// Serialized-history size that triggers automatic compaction at run start,
/// and the hard ceiling for outgoing messages.
const AUTO_COMPACT_BYTES: usize = 400 * 1024;
/// The same budget for a local server, which typically runs a far smaller
/// context window (8k-32k tokens) than a cloud model. llama.cpp/Ollama drop or
/// error past that window, and a 40-iteration run grows well beyond it. At
/// roughly 4 bytes per token, 96KB lands near 24k tokens, which leaves room
/// for the reply inside a 32k window.
const LOCAL_COMPACT_BYTES: usize = 96 * 1024;
/// Digest cap for a `fork`-mode sub-agent's copy of the parent conversation.
const FORK_DIGEST_BYTES: usize = 32 * 1024;

pub const COMPACT_PROMPT: &str = "You are a summarizer. Produce a concise but complete summary of the conversation so far: key decisions, changes made to the app and its data, current state, and open TODOs. This summary will REPLACE the prior messages as context, so preserve everything needed to continue the work.";

const SUBAGENT_SUFFIX: &str = "\n\nYou are an autonomous sub-agent. Complete the task fully and return a clear, self-contained final report as your final message. You cannot ask the user questions.";

/// One assistant run (or one sub-agent loop inside it).
#[derive(Clone)]
pub struct RunCtx {
    /// The chat session this loop reads and writes.
    pub session: String,
    /// The top-level run: equal to `session` for a user chat, the parent chat
    /// session for a sub-agent loop. It is the key in `app.ai_runs` and the
    /// session stamped on approval prompts, so `ai.stop {session}` reaches
    /// nested loops too.
    pub root: String,
    /// Shared with every nested loop: one stop flag per run tree.
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    /// 0 = user-facing chat, 1 = sub-agent (agent tools are removed there,
    /// which bounds recursion to depth 1).
    pub depth: usize,
}

impl RunCtx {
    pub fn root(session: String, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        RunCtx { root: session.clone(), session, stop, depth: 0 }
    }
    fn child(&self, session: String) -> Self {
        RunCtx {
            session,
            root: self.root.clone(),
            stop: self.stop.clone(),
            depth: self.depth + 1,
        }
    }
    fn stopped(&self) -> bool {
        // wasm: the worker is blocked inside this run, so ai.stop can't arrive
        // as an RPC — the shell raises a SharedArrayBuffer flag instead.
        #[cfg(target_arch = "wasm32")]
        if crate::wasm::stop_requested() {
            return true;
        }
        self.stop.load(Ordering::Relaxed)
    }
}

thread_local! {
    /// The loop running on this thread, so a tool (agent_run/agent_send) can
    /// see which session/run it belongs to. Sub-agent loops are synchronous on
    /// the same thread, so this is always the innermost loop.
    static CURRENT_RUN: std::cell::RefCell<Option<RunCtx>> = const { std::cell::RefCell::new(None) };
}

/// Restores the enclosing loop's ctx when a nested loop returns.
struct CtxGuard(Option<RunCtx>);
impl CtxGuard {
    fn enter(ctx: &RunCtx) -> CtxGuard {
        CtxGuard(CURRENT_RUN.with(|c| c.borrow_mut().replace(ctx.clone())))
    }
}
impl Drop for CtxGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT_RUN.with(|c| *c.borrow_mut() = prev);
    }
}

/// The loop a tool call belongs to. A tool invoked outside any run (a human
/// click through tools.call, or an app calling uapp.tool) gets a detached
/// top-level ctx on the main session.
fn current_ctx() -> RunCtx {
    CURRENT_RUN.with(|c| c.borrow().clone()).unwrap_or_else(|| {
        RunCtx::root("main".to_string(), Arc::new(std::sync::atomic::AtomicBool::new(false)))
    })
}

pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    /// None = provider default. For zai that means OMITTING max_tokens
    /// entirely: when the field is present, z.ai's Anthropic layer hands the
    /// whole budget to thinking and GLM-5.3 happily reasons until the cap
    /// with zero text/tool output (verified: 16384 -> 68k chars of pure
    /// thinking, nothing usable; omitted -> 48s, tool call, sane reasoning).
    /// Every working GLM client (../chat, Claude Code) omits max_tokens.
    pub max_tokens: Option<u32>,
    /// Ask the provider to stream. None = per-provider default: OFF for z.ai
    /// (its Anthropic-compatible endpoint drops tool-call blocks from streamed
    /// responses, so streamed GLM turns look like endless thinking and never
    /// act), ON for real Anthropic. Override via config `ai.stream`.
    pub stream: Option<bool>,
}

/// GLM-5.3 via z.ai's Anthropic-compatible endpoint treats a present
/// max_tokens as the thinking budget: given 16384 it reasoned for 68k chars
/// and returned NOTHING usable (and `thinking.budget_tokens` / `disabled`
/// are ignored by that layer). Omitting max_tokens entirely — what every
/// other working GLM client does — gets z.ai's own sane default and normal
/// behavior. So zai sends no cap unless the user sets `ai.max_tokens`.
/// Claude doesn't think unless explicitly enabled, so its cap is pure output.
fn default_max_tokens(provider: &str) -> u32 {
    match provider {
        "anthropic" => 32768,
        // A local server is usually a quantized model on consumer hardware
        // emitting a few tokens/sec, where 16384 tokens is over an hour of
        // generation for a single turn - and a thinking model will happily
        // spend all of it reasoning. Keep the cap low enough that a turn
        // actually finishes; raise it with `ai.max_tokens` in Settings.
        "local" => 2048,
        _ => 16384,
    }
}

/// Serialized-history budget for this provider: compaction trigger and the
/// outgoing hard-trim ceiling.
fn compact_bytes(cfg: &AiConfig) -> usize {
    if cfg.provider == "local" { LOCAL_COMPACT_BYTES } else { AUTO_COMPACT_BYTES }
}

fn load_config(app: &Arc<App>) -> Result<AiConfig> {
    let cfg = {
        let eng = app.engine.lock().unwrap();
        store::config_get(&eng.db, "ai")?
    }
    .ok_or_else(|| anyhow!("no AI provider configured — open Settings in the shell"))?;
    let provider = cfg["provider"].as_str().unwrap_or("").to_string();
    if provider.is_empty() {
        bail!("AI config missing provider");
    }
    let max_tokens = cfg["max_tokens"].as_u64().map(|v| v as u32);
    Ok(AiConfig {
        provider,
        api_key: cfg["api_key"].as_str().unwrap_or("").to_string(),
        model: cfg["model"].as_str().unwrap_or("").to_string(),
        base_url: cfg["base_url"].as_str().map(|s| s.to_string()),
        max_tokens,
        stream: cfg["stream"].as_bool(),
    })
}

fn system_prompt(app: &Arc<App>, ctx: &RunCtx) -> String {
    let (files, schema, name) = {
        let eng = app.engine.lock().unwrap();
        let files = store::query(&eng.db, "SELECT name, sz FROM sqlar ORDER BY name", &[])
            .map(|v| v.to_string())
            .unwrap_or_default();
        let schema = store::query(
            &eng.db,
            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'uapp_%' AND name NOT IN ('sqlar')",
            &[],
        )
        .map(|v| v.to_string())
        .unwrap_or_default();
        let name = store::meta_get(&eng.db, "name").ok().flatten().unwrap_or_default();
        (files, schema, name)
    };
    format!(
        r#"You are the built-in assistant of a uapp — a single-file shareable app (SQLite archive) named "{name}" used by a small team. You build and modify the app AND work with its data, on the user's behalf.

HOW THE APP WORKS
- The archive has exactly TWO top-level directories, and a file's location is its meaning: `app/` = the app itself (code and assets you write), `data/` = content the users put in (uploads, imports, exports). A path you write without one of those prefixes goes under `app/` automatically, so `write_file {{"name": "js/app.js"}}` stores `app/js/app.js`. To write user data, say so: `data/sales.csv`.
- The app's UI is plain HTML/CSS/JS stored as files in the archive. `app/index.html` is the entry point, shown in an iframe. It does NOT reload by itself when you write files: finish all the files of a change, then call `reload_app` once. The app's real runtime console output and uncaught errors (with stack traces) are captured for you — after `reload_app` or a run_js that exercises the UI, call `read_console` to check nothing threw. A `[console] …` note on a tool result means new errors are waiting; read them.
- Multi-file apps are fully supported: write as many .js/.css/asset files as the app needs (nested paths like js/app.js or css/style.css work). PREFER relative references from index.html (`src="js/app.js"`, `href="styles.css"`); root-absolute (`/js/app.js`) also works — both resolve inside `app/`. A data file is reachable at its own path (`/data/sales.csv`). All are served with correct content types. For small apps a single index.html is fine; split larger apps into modules so edits stay surgical. To CHANGE an existing file, use `edit_file` (exact string replacement) instead of rewriting it with write_file — reserve write_file for new files or total rewrites.
- App pages talk to the backend with `<script src="/uapp.js"></script>` which provides:
    uapp.query(sql, params?) -> {{columns, rows}}   // read-only SELECT; rows are OBJECTS keyed by column name ({{name: "Groceries"}}), never arrays
    uapp.exec(sql, params?)  -> {{changes, insertId}} // write; insertId = rowid the write created (null if none)
    uapp.whenVisible(el, cb)                        // run cb when el is first actually visible
    uapp.onChange(cb)                                // fires when any data changes (poll-synced from other users)
    uapp.action(name, opts, handler)                 // register business logic (REQUIRED — see below)
    uapp.call(name, input)                           // invoke one of the app's own actions
    uapp.tool(name, input)                           // the app can call ANY tool you can: sql_query, import_csv, download_lib, mcp__server__tool, app__action...
    uapp.toolbar.hide() / .show() / .toggle()        // the uapp bar around this app, for THIS window (saves nothing)
    uapp.panel.open(name)                            // a panel beside the app: chat, files, database, settings, tools
    uapp.user, uapp.device                           // attribution info
  Vanilla JS by default.

REQUIRED ARCHITECTURE — ACTIONS (non-negotiable)
Every app you build or modify MUST expose its business logic as named actions registered with uapp.action(). Registered actions automatically become tools YOU can call (named app__<name>), so the human clicking a button and you answering a chat request run the SAME code — one implementation of the rules, validation, and math.

  uapp.action("add_employee", {{
    description: "Add an employee. hired is YYYY-MM-DD. Returns {{ok}}.",
    params: {{ name: {{type: "string"}}, hired: {{type: "string", description: "YYYY-MM-DD"}} }},
  }}, async ({{name, hired}}) => {{
    if (!name || !hired) throw new Error("name and hired are required");
    await uapp.exec("INSERT INTO employees(name, hired) VALUES(?,?)", [name, hired]);
    return {{ok: true}};
  }});
  uapp.action("list_employees", {{description: "All employees with balances.", readonly: true, params: {{}}}},
    async () => (await uapp.query("SELECT * FROM employees")).rows);

Rules:
- Cover every meaningful operation: each create/update/delete, each report or computation a user would ask for. UI event handlers MUST go through uapp.call("action_name", {{...}}) — never inline business logic in onclick handlers.
- To get the new row's id after an INSERT, use `const {{insertId}} = await uapp.exec("INSERT ...")` or `INSERT ... RETURNING id` through exec (exec returns RETURNING rows). NEVER insert-then-SELECT-MAX(id) — that races with other devices' writes.
- Give each action a precise description and typed params (add "required: false" on optional ones). Mark pure reads readonly: true (they skip approval prompts).
- Actions run inside the open app page; they exist as tools only while the app is open in the shell. They re-register automatically on every reload.
- When the user asks for something an app__* action already covers, CALL THE ACTION instead of writing raw SQL — it carries the app's validation and business rules. Raw SQL is for exploration, migrations, and building new features.

LIBRARIES (Excel parsing, WASM modules, charting, etc.)
- Preferred: vendor the library INTO the archive with the download_lib tool, then load it with a relative path — the app stays self-contained, works offline, and the file ships to every user. Examples:
    download_lib {{"url": "https://cdn.sheetjs.com/xlsx-latest/package/dist/xlsx.full.min.js"}}  → <script src="vendor/xlsx.full.min.js"></script>
    download_lib for a .js + its .wasm sibling → fetch("vendor/foo.wasm") / WebAssembly.instantiateStreaming both work (served as application/wasm, same-origin).
  ESM builds work too: import from "./vendor/lib.mjs" inside a <script type="module">.
- Direct CDN <script src="https://..."> tags DO work but break when users are offline or on a locked-down network — use only when the user asks for it.

UI TOOLKIT — prefer these to re-authoring the same components and styles every app.
General UI + layout — pick ONE:
  - Plain modern HTML/CSS (default for simple apps): system-ui font stack, CSS custom properties for the palette, flexbox/grid layout, native <dialog>/<details>/<datalist>. Zero dependencies, nothing to vendor.
  - Bootstrap (MIT) — when the app needs a full component kit. Vendor two files and link them from index.html:
      download_lib {{"url": "https://cdn.jsdelivr.net/npm/bootstrap/dist/css/bootstrap.min.css"}}
      download_lib {{"url": "https://cdn.jsdelivr.net/npm/bootstrap/dist/js/bootstrap.bundle.min.js"}}
    Sidebar = offcanvas; also modals, dropdowns, tabs, forms, cards, buttons.
Data tables — Tabulator (MIT); use its Bootstrap 5 theme so it matches:
    download_lib {{"url": "https://cdn.jsdelivr.net/npm/tabulator-tables/dist/js/tabulator.min.js"}}
    download_lib {{"url": "https://cdn.jsdelivr.net/npm/tabulator-tables/dist/css/tabulator_bootstrap5.min.css"}}
  Sorting / filtering / inline-edit / CSV export are built in.
  Widgets that measure their container at init (Tabulator, Chart.js, ECharts) render 0 rows / 0-size when the container is hidden (display:none tab/section) — no error, just empty. Initialize them lazily: uapp.whenVisible(sectionEl, () => new Tabulator(...)) or on first tab switch.
3D / WebGL (three.js, custom GL, canvas games) — ALWAYS handle context loss. A phone's webview holds few GPU contexts and reclaims them under pressure, and every reload_app reloads the page; an app that ignores loss goes permanently blank (a white canvas, sometimes a broken-image square) and looks like your code crashed.
    canvas.addEventListener("webglcontextlost", (e) => {{ e.preventDefault(); }});      // WITHOUT preventDefault the context can never be restored
    canvas.addEventListener("webglcontextrestored", () => {{ /* recreate textures/buffers/programs, then resume the loop */ }});
  Keep enough state outside the GL objects to rebuild them, and hand the GPU memory back on the way out: uapp.onTeardown(() => renderer.dispose()).
Charts — Chart.js (MIT) for ordinary charts, ECharts (Apache-2.0) for large or dashboard-grade viz:
    download_lib {{"url": "https://cdn.jsdelivr.net/npm/chart.js/dist/chart.umd.js"}}
    download_lib {{"url": "https://cdn.jsdelivr.net/npm/echarts/dist/echarts.min.js"}}
Combobox — a native <input list="ids"> + <datalist>, or Tabulator's autocomplete editor in a grid.
Reach for these before writing custom CSS/JS for the same job — it keeps apps consistent and avoids rebuilding the same widgets. Plain HTML/CSS is still fine for simple apps.

CRITICAL DETERMINISM RULE
Every write (sql_exec / sql_batch / uapp.exec) is recorded in a log and REPLAYED on every user's device to reach the same state. Therefore writes must be deterministic:
- NEVER use datetime('now'), CURRENT_TIMESTAMP, random(), or similar inside write SQL. Compute timestamps/ids in JS (Date.now()) and pass them as parameters.
- Reads (sql_query / uapp.query) have no such restriction.
- Use CREATE TABLE IF NOT EXISTS so re-runs are safe.

CODE EXECUTION — run_js
You can execute JavaScript in the user's browser with the run_js tool. The code is the body of an async function receiving (uapp, loadScript); `return` a JSON-serializable value. console.log output is captured and returned too. Top-level declarations (e.g. `function render() {{}}`) are exposed as page globals after the run — the page's event handlers and later run_js calls can call them — and the result lists them under `exposed`. Error line numbers refer to your code as written (syntax errors include the offending line and an excerpt). Two contexts:
- "scratchpad" (default): a hidden empty page with the FULL uapp API but none of the app's code. Globals persist between calls (load a library once, use it in later calls). Use it to parse uploaded files, transform data, prototype logic, or verify a computation without touching the live app.
- "app": runs inside the LIVE app page — the document in the app frame (for a hosted site that is the site page itself; there is no separate editor frame). Inspect or manipulate its DOM and globals, call its actions via uapp.call(...), debug "why does the button not work". If your code navigates or reloads that page, the call ends with "the page unloaded" rather than a result — expected; run a fresh run_js against the new page.
Recipe — user uploads an Excel file:
  1. download_lib {{"url": "https://cdn.sheetjs.com/xlsx-latest/package/dist/xlsx.full.min.js"}}
  2. run_js: await loadScript("/vendor/xlsx.full.min.js");
             const f = await uapp.readFile("data/report.xlsx");           // {{b64, text}}
             const wb = XLSX.read(f.b64, {{type: "base64"}});
             return XLSX.utils.sheet_to_json(wb.Sheets[wb.SheetNames[0]]).slice(0, 5); // inspect first
  3. Then insert rows with uapp.exec(...) in a follow-up run_js (deterministic SQL, params only) or design tables first.
In the scratchpad, reference vendored files with ROOT-ABSOLUTE paths ("/vendor/lib.js") — relative paths resolve under /scratch/. run_js needs the app open in a browser; it fails headless.

RESEARCH & INTEGRATIONS
- web_search finds current information (no API key needed); fetch_url reads a page as text. Use them for reference data, format docs, rates, addresses — whenever the answer isn't in the app.
- If the user gives you an MCP endpoint (or asks to connect one), call add_mcp_server {{name, url, bearer_token?}} — its tools appear immediately as mcp__<name>__* and the config is saved for the whole team. remove_mcp_server disconnects it.

DATA IMPORT
When the user drags a file into the chat it is stored in the archive under `data/`. For CSV-like files call the import_csv tool (parses server-side into a real table as ONE efficient bulk write) — do NOT read big files into context and insert rows one by one. Then explore with sql_query.

APP FILES vs USER DATA
Users can export this app as a TEMPLATE (everything under `app/` + empty tables — no rows, no chat, no key); `data/` is left out. That is the whole rule, so put files on the right side of it: code, assets and any seed/reference dataset the app ships with go under `app/`; anything the users provide or you generate for them goes under `data/`. Moving a file between the two directories is how its role changes — there are no role tags.

STYLE
- Small-business users: friendly, concise, no jargon. Do the work rather than describing it.
- After changing app files, call `reload_app` and briefly say what changed.
- Actions run the code the live page loaded at its LAST reload. If you edit a script the page uses and then call an app__* action or run_js in the "app" context, the OLD code runs until you reload_app first. Order: edit → reload_app → call the action → (if it wrote pages) reload_app again to show the result.
- When you produce or want to show the user a file in the archive, call present_file — it opens in their viewer immediately.
- When a decision genuinely belongs to the user (destructive changes, ambiguous requirements, taste), call ask_user with concrete options instead of guessing or stalling.
- For a big, self-contained side quest (research a format, audit every table, build one module) delegate it with agent_run: the sub-agent works in its own conversation and hands you back a report, keeping this conversation small.
- All state lives in SQLite tables + archive files. There is no other storage.

CURRENT APP FILES: {files}
CURRENT DATA SCHEMA: {schema}"#
    ) + if ctx.depth > 0 { SUBAGENT_SUFFIX } else { "" }
        + if plan_mode(app) {
            "\n\nYou are in PLAN MODE (read-only). Do NOT modify data, files or config — \
             write tools are unavailable. Investigate with the read tools and propose a \
             concrete plan; the user will switch modes before you apply it."
        } else {
            ""
        }
}

// ---- tool definitions ------------------------------------------------------

/// Builtin tools from the registry (see tools.rs), minus disabled ones — and
/// minus the agent tools inside a sub-agent loop, which is what bounds
/// recursion to depth 1.
/// Plan mode = read-only toolset: gated builtins can write, and agents can
/// write inside their own loop, so both are withheld.
fn plan_mode(app: &Arc<App>) -> bool {
    app.ai_mode.lock().unwrap().as_str() == "plan"
}

fn builtin_tools(app: &Arc<App>, ctx: &RunCtx) -> Vec<Value> {
    let disabled = crate::tools::disabled_set(app);
    let plan = plan_mode(app);
    crate::tools::registry()
        .iter()
        .filter(|t| !disabled.contains(t.name))
        .filter(|t| ctx.depth == 0 || !crate::tools::is_agent_tool(t.name))
        .filter(|t| !plan || (!t.gated && !crate::tools::is_agent_tool(t.name)))
        .map(|t| {
            json!({"name": t.name, "description": t.description,
                   "input_schema": (t.schema)()})
        })
        .collect()
}

fn mcp_tools(app: &Arc<App>) -> Vec<Value> {
    if plan_mode(app) {
        return Vec::new(); // MCP side effects are unknown to us — all gated
    }
    let servers = rpc::mcp_servers_enabled(app);
    let disabled = crate::tools::disabled_set(app);
    crate::mcp::list_all_tools(&servers)
        .into_iter()
        .filter_map(|t| {
            let name = format!(
                "mcp__{}__{}",
                t["server"].as_str().unwrap_or(""),
                t["name"].as_str().unwrap_or("")
            );
            if disabled.contains(&name) {
                return None;
            }
            Some(json!({
                "name": name,
                "description": format!("[{} MCP] {}", t["server"].as_str().unwrap_or(""), t["description"].as_str().unwrap_or("")),
                "input_schema": t["input_schema"].clone(),
            }))
        })
        .collect()
}

/// Actions registered by the running app page (uapp.action) become tools
/// named app__<name>. Refreshed every loop iteration — the page re-registers
/// after each reload, so the set changes as the app is edited.
fn app_action_tools(app: &Arc<App>) -> Vec<Value> {
    let disabled = crate::tools::disabled_set(app);
    let plan = plan_mode(app);
    app.actions_snapshot()
        .into_iter()
        .filter_map(|(name, a)| {
            let tool_name = format!("app__{name}");
            if disabled.contains(&tool_name) || (plan && !a.readonly) {
                return None;
            }
            Some(json!({
                "name": tool_name,
                "description": format!("[app action{}] {}", if a.readonly { ", read-only" } else { "" }, a.description),
                "input_schema": a.schema,
            }))
        })
        .collect()
}

/// Gate a tool call behind user approval: in manual mode every gated tool
/// asks; calls with a `forced` reason (local database files, local network
/// hosts) ask in auto mode too. Returns true=run it, false=denied. Blocks the
/// AI thread until the user decides, the 5-minute timeout expires (deny), or
/// ai.stop fires.
fn approval_gate(
    app: &Arc<App>,
    ctx: &RunCtx,
    id: &str,
    name: &str,
    input: &Value,
    forced: &Option<(String, String)>,
) -> bool {
    match forced {
        None => {
            let manual = app.ai_mode.lock().unwrap().as_str() == "manual";
            if !manual || !crate::tools::is_gated(app, name) {
                return true;
            }
            if app.always_allow.lock().unwrap().contains(name) {
                return true;
            }
        }
        // Forced prompts honor their own, narrower always-allow key (e.g.
        // "fetch_url:local"), so approving OBS control once doesn't wave
        // through every future gated call of that tool.
        Some((key, _)) => {
            if app.always_allow.lock().unwrap().contains(key) {
                return true;
            }
        }
    }
    // wasm: the worker is blocked inside this run, so the shell can't answer
    // an RPC prompt — ask over the SharedArrayBuffer bridge instead (the main
    // thread shows the prompt and writes the decision back).
    #[cfg(target_arch = "wasm32")]
    {
        let reply = crate::wasm::bridge_call("approval", &json!({
            "id": id, "name": name, "input": input,
            "note": forced.as_ref().map(|(_, r)| r.clone()),
            "session": ctx.root,
        }));
        return match reply {
            Ok(v) => {
                let allow = v["allow"].as_bool().unwrap_or(false);
                if allow && v["always"].as_bool().unwrap_or(false) {
                    let key = forced.as_ref().map(|(k, _)| k.clone()).unwrap_or_else(|| name.to_string());
                    app.always_allow.lock().unwrap().insert(key);
                }
                allow
            }
            Err(_) => false,
        };
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
    let (tx, rx) = std::sync::mpsc::sync_channel::<(bool, bool)>(1);
    app.pending.lock().unwrap().insert(
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
    app.pending.lock().unwrap().remove(id);
    app.notify_pending();
    match decision {
        Ok((allow, always)) => {
            if allow && always {
                let key = forced.as_ref().map(|(k, _)| k.clone()).unwrap_or_else(|| name.to_string());
                app.always_allow.lock().unwrap().insert(key);
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
    let eng = app.engine.lock().unwrap();
    let app_id = eng.app_id.clone();
    drop(eng);

    // Check if this tool has been pre-approved for this app
    if store::check_app_approval(&app.engine.lock().unwrap().db, &app_id, tool) {
        return Ok(true);
    }

    // Need to prompt for approval. Generate a unique ID for this request.
    let id = format!("app-{}-{}", tool, fastrand::u32(..));

    // Get the app name for the prompt
    let app_name = {
        let eng = app.engine.lock().unwrap();
        store::meta_get(&eng.db, "name")?.unwrap_or_else(|| "uapp".into())
    };

    #[cfg(target_arch = "wasm32")]
    {
        // wasm: use the bridge to prompt
        let reply = crate::wasm::bridge_call("approval.app", &json!({
            "id": id,
            "app": app_name,
            "appId": app_id,
            "tool": tool,
            "input": input,
        }));
        match reply {
            Ok(v) => {
                let allow = v["allow"].as_bool().unwrap_or(false);
                if allow && v["always"].as_bool().unwrap_or(false) {
                    let eng = app.engine.lock().unwrap();
                    store::add_app_approval(&eng.db, &app_id, tool)?;
                }
                Ok(allow)
            }
            Err(_) => Ok(false),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(bool, bool)>(1);
        app.pending.lock().unwrap().insert(
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
        app.pending.lock().unwrap().remove(&id);
        app.notify_pending();
        match decision {
            Ok((allow, always)) => {
                if allow && always {
                    let eng = app.engine.lock().unwrap();
                    store::add_app_approval(&eng.db, &app_id, tool)?;
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
        let header: String = q["header"].as_str().unwrap_or("").trim().chars().take(12).collect();
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
        let reply = crate::wasm::bridge_call("ask_user", &json!({
            "id": id, "questions": questions, "session": ctx.root,
        }));
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
                .map(|c| c.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
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
    app.questions.lock().unwrap().insert(
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
    app.questions.lock().unwrap().remove(&id);
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
                    .map(|c| c.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
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

pub fn run_tool(app: &Arc<App>, name: &str, input: &Value) -> Result<Value> {
    if let Some(action) = name.strip_prefix("app__") {
        return app.invoke_action(action, input);
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        let (server, tool) = rest
            .split_once("__")
            .ok_or_else(|| anyhow!("bad mcp tool name"))?;
        let servers = rpc::mcp_servers_enabled(app);
        let srv = servers
            .iter()
            .find(|s| s["name"].as_str() == Some(server))
            .ok_or_else(|| anyhow!("unknown MCP server {server}"))?;
        return crate::mcp::call_tool(srv, tool, input.clone());
    }
    match crate::tools::registry().iter().find(|t| t.name == name) {
        Some(def) => (def.run)(app, input),
        None => bail!("unknown tool {name}"),
    }
}

// ---- chat history -> provider-neutral messages ------------------------------

/// A neutral message: role + blocks.
/// blocks: {type:"text",text} | {type:"image",media,b64}
///       | {type:"tool_use",id,name,input} | {type:"tool_result",id,content,is_error}
fn build_messages(app: &Arc<App>, session: &str, cap: usize) -> Result<Vec<(String, Vec<Value>)>> {
    let chat = rpc::dispatch(app, "chat.list", json!({"session": session}))?;
    let rows = chat["rows"].as_array().cloned().unwrap_or_default();
    let mut msgs: Vec<(String, Vec<Value>)> = Vec::new();
    let mut open_tool_ids: Vec<(String, String)> = Vec::new(); // (id, name)

    for row in &rows {
        let role = row[4].as_str().unwrap_or("");
        let content: Value =
            serde_json::from_str(row[5].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        let user = row[3].as_str().unwrap_or("");
        match role {
            "user" => {
                // Close any dangling tool calls (crashed run) before a new user turn.
                close_dangling(&mut msgs, &mut open_tool_ids);
                let mut blocks = Vec::new();
                let text = content["text"].as_str().unwrap_or("");
                let mut txt = if msgs.is_empty() || user.is_empty() {
                    text.to_string()
                } else {
                    format!("[{user}]: {text}")
                };
                if let Some(atts) = content["attachments"].as_array() {
                    for a in atts {
                        let name = a["name"].as_str().unwrap_or("");
                        if is_image(name) {
                            if let Some(b) = read_b64(app, name) {
                                blocks.push(json!({"type": "image", "media": media_type(name), "b64": b}));
                            }
                        }
                        txt.push_str(&format!("\n[attached file stored in archive at: {name}]"));
                    }
                }
                blocks.push(json!({"type": "text", "text": txt}));
                push_msg(&mut msgs, "user", blocks);
            }
            "assistant" => {
                close_dangling(&mut msgs, &mut open_tool_ids);
                let blocks: Vec<Value> = content["blocks"].as_array().cloned().unwrap_or_default();
                for b in &blocks {
                    if b["type"] == "tool_use" {
                        open_tool_ids.push((
                            b["id"].as_str().unwrap_or("").to_string(),
                            b["name"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                }
                if !blocks.is_empty() {
                    msgs.push(("assistant".into(), blocks));
                }
            }
            "tool" => {
                let results: Vec<Value> = content["results"].as_array().cloned().unwrap_or_default();
                for r in &results {
                    let id = r["id"].as_str().unwrap_or("");
                    open_tool_ids.retain(|(i, _)| i != id);
                }
                let blocks: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({"type": "tool_result", "id": r["id"], "name": r["name"],
                               "content": r["content"], "is_error": r["is_error"].as_bool().unwrap_or(false)})
                    })
                    .collect();
                push_msg(&mut msgs, "user", blocks);
            }
            // A compaction summary stands in for everything before it.
            "summary" => {
                close_dangling(&mut msgs, &mut open_tool_ids);
                let text = format!(
                    "[Conversation summary — earlier messages were compacted]\n{}",
                    content["text"].as_str().unwrap_or("")
                );
                push_msg(&mut msgs, "user", vec![json!({"type": "text", "text": text})]);
            }
            _ => {} // system/error rows are not sent to the model
        }
    }
    close_dangling(&mut msgs, &mut open_tool_ids);
    trim_tool_results(&mut msgs);
    hard_trim(&mut msgs, cap);
    Ok(msgs)
}

/// Non-destructive context trimming: walk tool_result blocks newest-first with
/// a byte budget and replace the ones that fall outside it (unless they're
/// small) with a note the model can act on. Only the OUTGOING copy changes —
/// the stored conversation keeps every byte.
fn trim_tool_results(msgs: &mut [(String, Vec<Value>)]) {
    let mut used = 0usize;
    for (_, blocks) in msgs.iter_mut().rev() {
        for b in blocks.iter_mut().rev() {
            if b["type"] != "tool_result" {
                continue;
            }
            let content = match b["content"].as_str() {
                Some(s) => s.to_string(),
                None => b["content"].to_string(),
            };
            if content.len() <= 1024 || used + content.len() <= OUTGOING_TOOL_BUDGET {
                used += content.len();
                continue;
            }
            let mut preview = content.clone();
            let mut end = 512.min(preview.len());
            while !preview.is_char_boundary(end) {
                end -= 1;
            }
            preview.truncate(end);
            b["content"] = json!(format!(
                "[older tool output trimmed — was {}KB. Re-run the tool with narrower \
                 parameters if needed. Preview: {}]",
                content.len().div_ceil(1024),
                preview
            ));
        }
    }
}

/// Last-resort ceiling: drop the OLDEST messages until the outgoing payload is
/// under the cap. Used when compaction can't help (the history is already one
/// summary plus a huge tail), so a run degrades instead of failing.
fn hard_trim(msgs: &mut Vec<(String, Vec<Value>)>, cap: usize) {
    // Sizes once, then a running total — re-serializing the whole history per
    // removed message made this quadratic on long conversations.
    let mut total: usize = msgs.iter().map(|(_, b)| json!(b).to_string().len()).sum();
    while msgs.len() > 2 && total > cap {
        total -= json!(msgs[0].1).to_string().len();
        msgs.remove(0);
        // The head must be a user turn with no orphaned tool_results: an
        // assistant-first payload and a tool_result without its tool_use are
        // both provider errors.
        while msgs.len() > 2 {
            if msgs[0].0 != "user" {
                total -= json!(msgs[0].1).to_string().len();
                msgs.remove(0);
                continue;
            }
            let before = json!(msgs[0].1).to_string().len();
            msgs[0].1.retain(|b| b["type"] != "tool_result");
            total = total.saturating_sub(before) + json!(msgs[0].1).to_string().len();
            if msgs[0].1.is_empty() {
                msgs.remove(0);
                continue;
            }
            break;
        }
    }
}

fn close_dangling(msgs: &mut Vec<(String, Vec<Value>)>, open: &mut Vec<(String, String)>) {
    if open.is_empty() {
        return;
    }
    let blocks: Vec<Value> = open
        .drain(..)
        .map(|(id, name)| {
            json!({"type": "tool_result", "id": id, "name": name,
                   "content": "(interrupted — no result recorded)", "is_error": true})
        })
        .collect();
    push_msg(msgs, "user", blocks);
}

fn push_msg(msgs: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some((last_role, last_blocks)) = msgs.last_mut() {
        if last_role == role {
            last_blocks.extend(blocks);
            return;
        }
    }
    msgs.push((role.into(), blocks));
}

fn is_image(name: &str) -> bool {
    let n = name.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| n.ends_with(e))
}

fn media_type(name: &str) -> &'static str {
    let n = name.to_lowercase();
    if n.ends_with(".png") { "image/png" }
    else if n.ends_with(".gif") { "image/gif" }
    else if n.ends_with(".webp") { "image/webp" }
    else { "image/jpeg" }
}

fn read_b64(app: &Arc<App>, name: &str) -> Option<String> {
    rpc::dispatch(app, "files.read", json!({"name": name}))
        .ok()
        .and_then(|r| r["b64"].as_str().map(|s| s.to_string()))
}

// ---- provider calls ----------------------------------------------------------

/// Returns assistant blocks in neutral form.
/// Parse a tool call's argument JSON as the provider sent it. A stream cut
/// off mid-arguments (or a model emitting junk) must NOT turn into a call with
/// empty arguments — `write_file {}` is far worse than no call — so the
/// unparsable text is kept under `_invalid` and the loop reports it instead
/// of running the tool.
fn tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => json!({"_invalid": raw}),
    }
}

/// Retry transient provider failures (rate limits, 5xx, dropped
/// connections) with a short backoff before giving up on the turn; a stop
/// request ends the wait early.
fn call_provider_retrying(
    app: &Arc<App>,
    ctx: &RunCtx,
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    const DELAYS_MS: [u64; 3] = [1_000, 4_000, 12_000];
    let mut attempt = 0;
    loop {
        match call_provider(app, &ctx.session, cfg, system, msgs, tools) {
            Ok(v) => return Ok(v),
            Err(e) => {
                let msg = e.to_string();
                // "stream read failed" is the response socket dying mid-SSE —
                // seen on every mobile background/resume cycle (the OS freezes
                // the process and kills its sockets) and on wifi↔cell handoffs.
                // The request is re-sent from scratch, so it retries cleanly.
                let transient = msg.starts_with("provider request failed")
                    || msg.starts_with("stream read failed")
                    || ["provider returned 408", "provider returned 429", "provider returned 5"]
                        .iter()
                        .any(|p| msg.starts_with(p));
                if !transient || attempt >= DELAYS_MS.len() || ctx.stopped() {
                    return Err(e);
                }
                app.notify("ai", json!({"state": "retry", "session": ctx.session,
                    "message": format!("{msg} — retrying in {}s", DELAYS_MS[attempt] / 1000)}));
                let _ = app.wait_until(DELAYS_MS[attempt], 250, || ctx.stopped());
                attempt += 1;
            }
        }
    }
}

fn call_provider(
    app: &Arc<App>,
    session: &str,
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    match cfg.provider.as_str() {
        // z.ai coding-plan keys only work on the Anthropic-compatible
        // endpoint (paas/v4 rejects them with "insufficient balance"),
        // and API-plan keys work there too — so zai always speaks
        // Anthropic wire format. Streamed: GLM-5.3 thinks for a long time
        // and a non-streaming request looks like a stall (or times out).
        "anthropic" | "zai" => call_anthropic(app, session, cfg, system, msgs, tools),
        // "local" is any OpenAI-compatible server on this machine (Ollama, LM
        // Studio, llama.cpp's server, vLLM, text-generation-webui, ...) — they
        // only implement the older chat/completions shape, same as OpenRouter.
        "openrouter" | "local" => call_openai_compat(app, session, cfg, system, msgs, tools),
        // Direct OpenAI gets their newer Responses API (see call_openai_responses).
        "openai" => call_openai_responses(cfg, system, msgs, tools),
        other => bail!("unknown provider '{other}' (use anthropic, zai, openrouter, openai or local)"),
    }
}

/// Overall request timeout for a non-streamed provider call. This is the whole
/// blocking POST — nothing comes back until the model is completely done — so
/// it must cover a full generation. A local server can be genuinely slow (a
/// big model on CPU may emit only a few tokens/sec), where a long answer takes
/// many minutes; 5 minutes was far too tight and cut off any non-trivial
/// request. Local therefore gets a very generous ceiling; cloud providers keep
/// the tighter one. `UAPP_AI_TIMEOUT_SECS` overrides both.
fn ai_timeout(cfg: &AiConfig) -> std::time::Duration {
    let secs = std::env::var("UAPP_AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(if cfg.provider == "local" { 3600 } else { 300 });
    std::time::Duration::from_secs(secs)
}

/// One place to turn a failed provider HTTP response into an error, so the
/// message shape ("provider returned 429: …") is identical on every path.
fn provider_error(resp: crate::net::Resp) -> anyhow::Error {
    let code = resp.status;
    let body = resp.into_string().unwrap_or_default();
    let msg = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()).map(String::from))
        .unwrap_or_else(|| body.chars().take(400).collect());
    anyhow!("provider returned {code}: {msg}")
}

/// Anthropic `system` as cacheable blocks: the big stable prompt (instructions
/// + tool docs) is one cached block, the volatile tail (current files/schema,
/// which change as work proceeds) a second uncached one — so the cached prefix
/// stays valid across a turn's many iterations. `cache=false` (z.ai) returns
/// the plain string unchanged.
fn system_blocks(system: &str, cache: bool) -> Value {
    if !cache {
        return json!(system);
    }
    let split = system.find("\n\nCURRENT APP FILES:").unwrap_or(system.len());
    let (stable, volatile) = system.split_at(split);
    let mut blocks = vec![json!({
        "type": "text", "text": stable,
        "cache_control": {"type": "ephemeral"},
    })];
    if !volatile.is_empty() {
        blocks.push(json!({"type": "text", "text": volatile}));
    }
    json!(blocks)
}

/// Mark the last tool with `cache_control`, so the whole (stable) tools array
/// caches with the system prefix.
fn cache_last_tool(tools: &[Value], cache: bool) -> Value {
    if !cache || tools.is_empty() {
        return json!(tools);
    }
    let mut out = tools.to_vec();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    json!(out)
}

/// Cache the conversation prefix by marking the last content block of the last
/// message: everything before this turn's tail is reused on the next
/// iteration instead of re-billed.
fn cache_last_message(messages: &[Value], cache: bool) -> Value {
    if !cache || messages.is_empty() {
        return json!(messages);
    }
    let mut out = messages.to_vec();
    if let Some(content) = out.last_mut().and_then(|m| m["content"].as_array_mut()) {
        if let Some(block) = content.last_mut() {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    json!(out)
}

fn http_post(url: &str, headers: &[(&str, &str)], body: Value, timeout: std::time::Duration) -> Result<Value> {
    let mut hdrs = headers.to_vec();
    hdrs.push(("content-type", "application/json"));
    let resp = crate::net::request("POST", url, &hdrs, Some(&body.to_string()), 20, timeout.as_secs())
        .map_err(|e| anyhow!("provider request failed: {e}"))?;
    if resp.status >= 400 {
        return Err(provider_error(resp));
    }
    Ok(serde_json::from_str(&resp.into_string()?)?)
}

/// Extract neutral blocks from a complete (non-streamed) Anthropic message.
/// Lenient — returns an empty vec when there is nothing usable (e.g. the whole
/// budget went to reasoning); the caller decides whether that's worth a
/// budget-bump retry or an error.
fn parse_anthropic_message(resp: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for b in resp["content"].as_array().cloned().unwrap_or_default() {
        match b["type"].as_str().unwrap_or("") {
            "text" => out.push(json!({"type": "text", "text": b["text"]})),
            "tool_use" => out.push(json!({"type": "tool_use", "id": b["id"],
                "name": b["name"], "input": b["input"]})),
            "thinking" => out.push(json!({"type": "thinking",
                "thinking": b["thinking"], "signature": b["signature"]})),
            _ => {}
        }
    }
    if !out.iter().any(|b| b["type"] == "text" || b["type"] == "tool_use") {
        out.clear();
    }
    out
}

/// Read an Anthropic SSE stream: assemble content blocks from deltas, pushing
/// partial text and thinking heartbeats to the shell as they arrive.
/// Returns (blocks, stop_reason, thinking_chars).
fn read_anthropic_sse(
    app: &Arc<App>,
    session: &str,
    reader: Box<dyn std::io::BufRead>,
) -> Result<(Vec<Value>, String, usize)> {
    #[derive(Default)]
    struct Blk {
        typ: String,
        text: String,
        sig: String,
        id: String,
        name: String,
        json: String,
        start_input: Value,
    }
    let mut blks: std::collections::BTreeMap<u64, Blk> = Default::default();
    let mut live_text = String::new();
    let mut think_chars: usize = 0;
    let mut stop_reason = String::new();
    let mut last_push: u64 = 0;
    use std::io::BufRead as _;
    for line in reader.lines() {
        let line = line.map_err(|e| anyhow!("stream read failed: {e}"))?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else { continue };
        match ev["type"].as_str().unwrap_or("") {
            "content_block_start" => {
                let idx = ev["index"].as_u64().unwrap_or(0);
                let cb = &ev["content_block"];
                blks.insert(idx, Blk {
                    typ: cb["type"].as_str().unwrap_or("").to_string(),
                    text: cb["text"].as_str().unwrap_or("").to_string(),
                    id: cb["id"].as_str().unwrap_or("").to_string(),
                    name: cb["name"].as_str().unwrap_or("").to_string(),
                    start_input: cb["input"].clone(),
                    ..Default::default()
                });
            }
            "content_block_delta" => {
                let idx = ev["index"].as_u64().unwrap_or(0);
                let d = &ev["delta"];
                let blk = blks.entry(idx).or_default();
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        blk.text.push_str(t);
                        live_text.push_str(t);
                        if crate::store::now_ms().saturating_sub(last_push) > 250 {
                            app.notify("ai", json!({"state": "streaming", "text": live_text,
                                                    "session": session}));
                            last_push = crate::store::now_ms();
                        }
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        blk.text.push_str(t);
                        think_chars += t.len();
                        // Heartbeat so long reasoning doesn't look stalled.
                        if crate::store::now_ms().saturating_sub(last_push) > 900 {
                            app.notify("ai", json!({"state": "thinking", "chars": think_chars,
                                                    "session": session}));
                            last_push = crate::store::now_ms();
                        }
                    }
                    "signature_delta" => blk.sig.push_str(d["signature"].as_str().unwrap_or("")),
                    "input_json_delta" => blk.json.push_str(d["partial_json"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = ev["delta"]["stop_reason"].as_str() {
                    stop_reason = sr.to_string();
                }
            }
            "error" => bail!("provider stream error: {}",
                ev["error"]["message"].as_str().unwrap_or("unknown")),
            "message_stop" => break,
            _ => {}
        }
    }
    if !live_text.is_empty() {
        app.notify("ai", json!({"state": "streaming", "text": live_text, "session": session}));
    }

    let mut out = Vec::new();
    for (_, b) in blks {
        match b.typ.as_str() {
            "text" if !b.text.is_empty() => out.push(json!({"type": "text", "text": b.text})),
            "thinking" => out.push(json!({"type": "thinking", "thinking": b.text, "signature": b.sig})),
            "tool_use" => {
                let input = if b.json.trim().is_empty() {
                    if b.start_input.is_object() { b.start_input } else { json!({}) }
                } else {
                    tool_input(&b.json)
                };
                out.push(json!({"type": "tool_use", "id": b.id, "name": b.name, "input": input}));
            }
            _ => {}
        }
    }
    Ok((out, stop_reason, think_chars))
}

fn call_anthropic(
    app: &Arc<App>,
    session: &str,
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    let base = cfg.base_url.clone().unwrap_or_else(|| match cfg.provider.as_str() {
        "zai" => "https://api.z.ai/api/anthropic".into(),
        _ => "https://api.anthropic.com".into(),
    });
    let default_model = match cfg.provider.as_str() {
        "zai" => "glm-5.3",
        _ => "claude-sonnet-5",
    };
    // Replay thinking blocks ONLY for the final assistant turn (the one whose
    // tool results follow) so the model can resume mid-reasoning — that's the
    // Anthropic tool-use rule. Older turns' thinking is stripped: replaying a
    // whole run's reasoning makes GLM re-read and extend every prior thought,
    // so thinking snowballs each iteration until it eats the entire budget.
    let last_assistant = msgs.iter().rposition(|(r, _)| r == "assistant");
    let messages: Vec<Value> = msgs
        .iter()
        .enumerate()
        .map(|(i, (role, blocks))| {
            let content: Vec<Value> = blocks
                .iter()
                .filter_map(|b| match b["type"].as_str().unwrap_or("") {
                    "text" => Some(json!({"type": "text", "text": b["text"]})),
                    "image" => Some(json!({"type": "image", "source": {"type": "base64",
                        "media_type": b["media"], "data": b["b64"]}})),
                    "tool_use" => Some(json!({"type": "tool_use", "id": b["id"],
                        "name": b["name"], "input": b["input"]})),
                    "tool_result" => Some(json!({"type": "tool_result", "tool_use_id": b["id"],
                        "content": b["content"], "is_error": b["is_error"]})),
                    // Extended-thinking blocks must be replayed verbatim
                    // (GLM-5.3 and Claude both emit them with tool use) —
                    // but only for the final assistant turn, see above.
                    "thinking" if Some(i) == last_assistant => Some(json!({"type": "thinking",
                        "thinking": b["thinking"], "signature": b["signature"]})),
                    _ => None,
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect();
    // z.ai's Anthropic-compatible endpoint mis-streams GLM: tool-call blocks
    // never arrive as deltas, so a streamed turn looks like endless thinking
    // that eats the whole budget without ever acting (the ../chat app hit the
    // same thing and drives z.ai non-streaming for exactly this reason).
    // Non-streaming returns tool calls reliably, so zai defaults to it; real
    // Anthropic streams. Override per instance with config `ai.stream`.
    let stream = cfg.stream.unwrap_or(cfg.provider != "zai");

    // Thinking models can exhaust an explicit token budget before emitting
    // any text or tool call. When that happens, retry once with a doubled
    // budget before giving up. zai defaults to NO cap (see AiConfig) — the
    // mere presence of max_tokens makes z.ai hand the whole budget to
    // thinking — so the retry only applies when a cap was requested.
    let mut budget = match cfg.provider.as_str() {
        "zai" => cfg.max_tokens, // None = omit the field entirely
        _ => Some(cfg.max_tokens.unwrap_or_else(|| default_max_tokens(&cfg.provider))),
    };
    // Prompt caching pays off across a long tool loop, but only real
    // Anthropic is guaranteed to honour `cache_control`; z.ai's compatible
    // endpoint is left alone.
    let cache = cfg.provider == "anthropic";
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut body = json!({
            "model": if cfg.model.is_empty() { default_model } else { &cfg.model },
            "system": system_blocks(system, cache),
            "messages": cache_last_message(&messages, cache),
            "tools": cache_last_tool(tools, cache),
        });
        if let Some(b) = budget {
            body["max_tokens"] = json!(b);
        }
        if stream {
            body["stream"] = json!(true);
        }
        // z.ai accepts either header; real Anthropic uses x-api-key. Send both.
        let auth = format!("Bearer {}", cfg.api_key);
        let mut headers: Vec<(&str, &str)> = vec![
            ("x-api-key", &cfg.api_key),
            ("Authorization", &auth),
            ("anthropic-version", "2023-06-01"),
            ("content-type", "application/json"),
            ("accept", if stream { "text/event-stream" } else { "application/json" }),
        ];
        // Browser build: Anthropic requires this opt-in header before it
        // answers cross-origin requests from a page.
        if cfg!(target_arch = "wasm32") {
            headers.push(("anthropic-dangerous-direct-browser-access", "true"));
        }
        // Streaming: no overall timeout (long thinking runs are legitimate),
        // but a 180s idle-read timeout so a dead connection can't hang.
        // Non-streaming: nothing arrives until the model is done, so the read
        // timeout must cover a full long-thinking generation.
        let resp = crate::net::request(
            "POST",
            &format!("{base}/v1/messages"),
            &headers,
            Some(&body.to_string()),
            20,
            if stream { 180 } else { 900 },
        )
        .map_err(|e| anyhow!("provider request failed: {e}"))?;
        if resp.status >= 400 {
            return Err(provider_error(resp));
        }
        let (out, stop_reason, think_chars) = if resp.content_type.contains("event-stream") {
            read_anthropic_sse(app, session, resp.reader)?
        } else {
            // One JSON message: we asked non-streaming (the zai default) or
            // the provider ignored the stream flag.
            let v: Value = serde_json::from_str(&resp.into_string()?)?;
            if v.get("error").is_some() {
                bail!("provider error: {}", v["error"]["message"].as_str().unwrap_or("unknown"));
            }
            let stop = v["stop_reason"].as_str().unwrap_or("").to_string();
            (parse_anthropic_message(&v), stop, 0)
        };
        if out.iter().any(|b| b["type"] == "text" || b["type"] == "tool_use") {
            return Ok(out);
        }
        // Nothing usable. Thinking-only + max_tokens = an explicit budget was
        // exhausted by reasoning; give it one more shot with double the room.
        if stop_reason == "max_tokens" && attempt == 1 && budget.is_some() {
            let old = budget.unwrap();
            budget = Some(old.saturating_mul(2).min(131_072));
            eprintln!(
                "uapp: model spent the whole {}-token budget thinking ({} chars); retrying with {}",
                old, think_chars, budget.unwrap()
            );
            app.notify("ai", json!({"state": "running", "session": session,
                "note": format!("model needed more room to think — retrying with {} tokens", budget.unwrap())}));
            continue;
        }
        if stop_reason == "max_tokens" && budget.is_some() {
            bail!(
                "the model used the entire {}-token budget on internal reasoning without \
                 producing an answer — raise max_tokens in Settings or simplify the request",
                budget.unwrap()
            );
        }
        bail!("empty response from provider (stop_reason: {stop_reason})");
    }
}

/// Read an OpenAI chat/completions SSE stream (the shape Ollama, LM Studio,
/// llama.cpp's server, vLLM, OpenRouter all emit). Accumulates streamed text
/// (pushed to the shell as it arrives, like the Anthropic path) and tool-call
/// fragments — `tool_calls[]` deltas carry an `index`; `id`/`name` appear once,
/// `arguments` arrive as string fragments to concatenate — then assembles the
/// same neutral blocks the non-streamed branch returns.
/// Returns the neutral blocks, the final `finish_reason`, and how many chars
/// of `reasoning_content` streamed by. The last two let the caller tell a model
/// that ran out of room mid-thought from one that genuinely returned nothing.
fn read_openai_sse(
    app: &Arc<App>,
    session: &str,
    reader: Box<dyn std::io::BufRead>,
) -> Result<(Vec<Value>, String, usize)> {
    #[derive(Default)]
    struct Call {
        id: String,
        name: String,
        args: String,
    }
    let mut text = String::new();
    let mut live_text = String::new();
    let mut calls: std::collections::BTreeMap<u64, Call> = Default::default();
    let mut last_push: u64 = 0;
    let mut think_chars: usize = 0;
    let mut last_think_push: u64 = 0;
    let mut finish = String::new();
    use std::io::BufRead as _;
    for line in reader.lines() {
        let line = line.map_err(|e| anyhow!("stream read failed: {e}"))?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };
        // Some servers surface errors mid-stream as a JSON object with `error`.
        if let Some(msg) = ev["error"]["message"].as_str() {
            bail!("provider stream error: {msg}");
        }
        if let Some(f) = ev["choices"][0]["finish_reason"].as_str() {
            if !f.is_empty() {
                finish = f.to_string();
            }
        }
        let delta = &ev["choices"][0]["delta"];
        // Thinking models (Qwen3, DeepSeek-R1, GLM) stream reasoning in a
        // separate field. It is not part of the answer and must not be
        // concatenated into it, but it has to be observed: without it a long
        // think looks like a stalled connection, and a turn that spends its
        // whole budget reasoning would come back as a bare "empty response".
        if let Some(r) = delta["reasoning_content"].as_str() {
            if !r.is_empty() {
                think_chars += r.len();
                // Heartbeat so long reasoning doesn't look stalled.
                if crate::store::now_ms().saturating_sub(last_think_push) > 900 {
                    app.notify("ai", json!({"state": "thinking", "chars": think_chars,
                                            "session": session}));
                    last_think_push = crate::store::now_ms();
                }
            }
        }
        if let Some(t) = delta["content"].as_str() {
            if !t.is_empty() {
                text.push_str(t);
                live_text.push_str(t);
                if crate::store::now_ms().saturating_sub(last_push) > 250 {
                    app.notify("ai", json!({"state": "streaming", "text": live_text, "session": session}));
                    last_push = crate::store::now_ms();
                }
            }
        }
        for tc in delta["tool_calls"].as_array().cloned().unwrap_or_default() {
            let idx = tc["index"].as_u64().unwrap_or(0);
            let call = calls.entry(idx).or_default();
            if let Some(id) = tc["id"].as_str() {
                if !id.is_empty() {
                    call.id = id.to_string();
                }
            }
            if let Some(n) = tc["function"]["name"].as_str() {
                if !n.is_empty() {
                    call.name = n.to_string();
                }
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                call.args.push_str(a);
            }
        }
    }
    if !live_text.is_empty() {
        app.notify("ai", json!({"state": "streaming", "text": live_text, "session": session}));
    }

    let mut out = Vec::new();
    if !text.is_empty() {
        out.push(json!({"type": "text", "text": text}));
    }
    for (i, c) in calls {
        let args = tool_input(&c.args);
        // A tool call needs an id for the follow-up tool_result to reference;
        // synthesize a stable one if the server omitted it.
        let id = if c.id.is_empty() { format!("call_{i}") } else { c.id };
        out.push(json!({"type": "tool_use", "id": id, "name": c.name, "input": args}));
    }
    Ok((out, finish, think_chars))
}

fn call_openai_compat(
    app: &Arc<App>,
    session: &str,
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    let base = cfg.base_url.clone().unwrap_or_else(|| match cfg.provider.as_str() {
        "zai" => "https://api.z.ai/api/paas/v4".into(),
        "openrouter" => "https://openrouter.ai/api/v1".into(),
        // Ollama's OpenAI-compatible endpoint, by far the most common local
        // setup. LM Studio, llama.cpp's server, vLLM etc. all listen on a
        // different port — Base URL in Settings overrides this.
        "local" => "http://localhost:11434/v1".into(),
        _ => "https://api.openai.com/v1".into(),
    });
    let default_model = match cfg.provider.as_str() {
        "zai" => "glm-4.6",
        "openrouter" => "anthropic/claude-sonnet-4.5",
        _ => "gpt-4o",
    };
    // Cloud providers have one sane default model; a local server can be
    // running anything, so there's no default worth guessing — the name must
    // match a model already pulled/loaded there.
    if cfg.provider == "local" && cfg.model.is_empty() {
        bail!(
            "set Model in Settings to the exact name of a model already loaded on your \
             local server (e.g. \"qwen3.8\" for Ollama)"
        );
    }
    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": system})];
    for (role, blocks) in msgs {
        if role == "assistant" {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "text" => text.push_str(b["text"].as_str().unwrap_or("")),
                    "tool_use" => tool_calls.push(json!({
                        "id": b["id"], "type": "function",
                        "function": {"name": b["name"], "arguments": b["input"].to_string()}
                    })),
                    _ => {}
                }
            }
            let mut m = json!({"role": "assistant"});
            m["content"] = if text.is_empty() { Value::Null } else { json!(text) };
            if !tool_calls.is_empty() {
                m["tool_calls"] = json!(tool_calls);
            }
            messages.push(m);
        } else {
            // user turn: tool results become role:"tool" messages first
            let mut parts = Vec::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "tool_result" => messages.push(json!({
                        "role": "tool", "tool_call_id": b["id"],
                        "content": b["content"].as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| b["content"].to_string()),
                    })),
                    "text" => parts.push(json!({"type": "text", "text": b["text"]})),
                    "image" => parts.push(json!({"type": "image_url", "image_url": {"url":
                        format!("data:{};base64,{}", b["media"].as_str().unwrap_or("image/png"),
                                b["b64"].as_str().unwrap_or(""))}})),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                messages.push(json!({"role": "user", "content": parts}));
            }
        }
    }
    let oa_tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({"type": "function", "function": {
                "name": t["name"], "description": t["description"],
                "parameters": t["input_schema"]}})
        })
        .collect();
    // Stream local servers by default: a slow local model (a few tokens/sec on
    // CPU) otherwise looks stalled for minutes, and streaming turns the overall
    // deadline into an idle-read one so a long-but-progressing generation never
    // trips it. OpenRouter/OpenAI-compat stay non-streaming unless `ai.stream`
    // opts in. Override per instance with config `ai.stream`.
    let stream = cfg.stream.unwrap_or(cfg.provider == "local");
    // Thinking models can burn an entire token budget on reasoning before
    // emitting any text or tool call. The Anthropic path already retries once
    // with a doubled budget for exactly that failure; do the same here, since
    // local servers run precisely those models (Qwen3, DeepSeek-R1, GLM).
    let mut budget = cfg.max_tokens.unwrap_or_else(|| default_max_tokens(&cfg.provider));
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut body = json!({
            "model": if cfg.model.is_empty() { default_model } else { cfg.model.as_str() },
            "messages": messages.clone(),
            "tools": oa_tools.clone(),
            "max_tokens": budget,
        });

        let (out, finish, think_chars) = if !stream {
            let resp = http_post(
                &format!("{base}/chat/completions"),
                &[
                    ("Authorization", &format!("Bearer {}", cfg.api_key)),
                    ("content-type", "application/json"),
                ],
                body,
                ai_timeout(cfg),
            )?;
            let m = &resp["choices"][0]["message"];
            let mut out = Vec::new();
            if let Some(t) = m["content"].as_str() {
                if !t.is_empty() {
                    out.push(json!({"type": "text", "text": t}));
                }
            }
            for tc in m["tool_calls"].as_array().cloned().unwrap_or_default() {
                let args = tool_input(tc["function"]["arguments"].as_str().unwrap_or(""));
                out.push(json!({"type": "tool_use", "id": tc["id"],
                    "name": tc["function"]["name"], "input": args}));
            }
            let finish = resp["choices"][0]["finish_reason"].as_str().unwrap_or("").to_string();
            let think = m["reasoning_content"].as_str().map(|t| t.len()).unwrap_or(0);
            (out, finish, think)
        } else {
            body["stream"] = json!(true);
            // Ask for usage in the final chunk too (harmless if the server ignores it).
            body["stream_options"] = json!({"include_usage": true});
            // Streaming: the read timeout is per-chunk (idle), not overall, so a model
            // that keeps emitting tokens can run as long as it needs; ai_timeout is a
            // generous ceiling on any single silent gap (covers slow prompt prefill).
            let auth = format!("Bearer {}", cfg.api_key);
            let resp = crate::net::request(
                "POST",
                &format!("{base}/chat/completions"),
                &[
                    ("Authorization", &auth),
                    ("content-type", "application/json"),
                    ("accept", "text/event-stream"),
                ],
                Some(&body.to_string()),
                20,
                ai_timeout(cfg).as_secs(),
            )
            .map_err(|e| anyhow!("provider request failed: {e}"))?;
            if resp.status >= 400 {
                return Err(provider_error(resp));
            }
            read_openai_sse(app, session, resp.reader)?
        };

        if !out.is_empty() {
            return Ok(out);
        }
        // Nothing usable. If the model was still reasoning when it hit the cap,
        // one retry with double the room usually lands an answer.
        if finish == "length" && think_chars > 0 && attempt == 1 {
            let old = budget;
            budget = budget.saturating_mul(2).min(131_072);
            eprintln!(
                "uapp: model spent the whole {old}-token budget thinking ({think_chars} chars); \
                 retrying with {budget}"
            );
            app.notify("ai", json!({"state": "running", "session": session,
                "note": format!("model needed more room to think — retrying with {budget} tokens")}));
            continue;
        }
        if finish == "length" && think_chars > 0 {
            bail!(
                "the model used the entire {budget}-token budget on internal reasoning \
                 without producing an answer — raise max_tokens in Settings or simplify \
                 the request"
            );
        }
        bail!("empty response from provider (finish_reason: {finish})");
    }
}

/// OpenAI's Responses API (POST {base}/responses) — the endpoint OpenAI now
/// recommends over Chat Completions for tool-using/agentic workloads. Used
/// only for direct OpenAI calls: OpenRouter and local servers (Ollama, LM
/// Studio, llama.cpp's server, vLLM, ...) only implement the older
/// chat/completions shape, so they stay on call_openai_compat.
fn call_openai_responses(
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    let base = cfg.base_url.clone().unwrap_or_else(|| "https://api.openai.com/v1".into());
    let default_model = "gpt-5.1";

    // Unlike chat/completions, the system prompt is its own top-level field
    // ("instructions"), not an input item.
    let mut input: Vec<Value> = Vec::new();
    for (role, blocks) in msgs {
        if role == "assistant" {
            let mut text = String::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "text" => text.push_str(b["text"].as_str().unwrap_or("")),
                    // Each tool call is its own top-level input item here
                    // (not nested in the message), matching how the API
                    // hands them back in `output`.
                    "tool_use" => input.push(json!({
                        "type": "function_call", "call_id": b["id"],
                        "name": b["name"], "arguments": b["input"].to_string(),
                    })),
                    // Reasoning items are optional to replay; the model
                    // reasons fresh each turn when they're omitted, so we
                    // drop them rather than round-trip encrypted content.
                    _ => {}
                }
            }
            if !text.is_empty() {
                input.push(json!({"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]}));
            }
        } else {
            let mut parts = Vec::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "tool_result" => input.push(json!({
                        "type": "function_call_output", "call_id": b["id"],
                        "output": b["content"].as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| b["content"].to_string()),
                    })),
                    "text" => parts.push(json!({"type": "input_text", "text": b["text"]})),
                    "image" => parts.push(json!({"type": "input_image", "image_url":
                        format!("data:{};base64,{}", b["media"].as_str().unwrap_or("image/png"),
                                b["b64"].as_str().unwrap_or(""))})),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                input.push(json!({"type": "message", "role": "user", "content": parts}));
            }
        }
    }

    // Responses API tools are flat (no nested "function" key, unlike
    // chat/completions).
    let oa_tools: Vec<Value> = tools
        .iter()
        .map(|t| json!({"type": "function", "name": t["name"],
            "description": t["description"], "parameters": t["input_schema"]}))
        .collect();

    let body = json!({
        "model": if cfg.model.is_empty() { default_model } else { cfg.model.as_str() },
        "instructions": system,
        "input": input,
        "tools": oa_tools,
        "max_output_tokens": cfg.max_tokens.unwrap_or_else(|| default_max_tokens(&cfg.provider)),
        // Every device shares one chat history in the .uapp file already —
        // OpenAI's own server-side conversation storage would be redundant.
        "store": false,
    });
    let resp = http_post(
        &format!("{base}/responses"),
        &[
            ("Authorization", &format!("Bearer {}", cfg.api_key)),
            ("content-type", "application/json"),
        ],
        body,
        ai_timeout(cfg),
    )?;
    if resp["status"].as_str() == Some("failed") {
        bail!("provider error: {}", resp["error"]["message"].as_str().unwrap_or("unknown"));
    }
    let mut out = Vec::new();
    for item in resp["output"].as_array().cloned().unwrap_or_default() {
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                for c in item["content"].as_array().cloned().unwrap_or_default() {
                    if c["type"] == "output_text" {
                        if let Some(t) = c["text"].as_str() {
                            if !t.is_empty() {
                                out.push(json!({"type": "text", "text": t}));
                            }
                        }
                    }
                }
            }
            "function_call" => {
                let args = tool_input(item["arguments"].as_str().unwrap_or(""));
                out.push(json!({"type": "tool_use", "id": item["call_id"],
                    "name": item["name"], "input": args}));
            }
            // Surfaced as a "thinking" block so the chat UI's existing
            // thinking-line renderer picks it up; not replayed back (see above).
            "reasoning" => {
                let summary: String = item["summary"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|s| s["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !summary.is_empty() {
                    out.push(json!({"type": "thinking", "thinking": summary, "signature": ""}));
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        bail!("empty response from provider: {}", resp.to_string().chars().take(300).collect::<String>());
    }
    Ok(out)
}

// ---- the loop -----------------------------------------------------------------

/// What a finished loop produced: how many provider turns it took and the
/// assistant's final text (a sub-agent's report).
struct Outcome {
    steps: usize,
    report: String,
}

pub fn run(app: &Arc<App>, ctx: &RunCtx) -> Result<()> {
    run_loop(app, ctx, MAX_ITERATIONS).map(|_| ())
}

fn run_loop(app: &Arc<App>, ctx: &RunCtx, max_iters: usize) -> Result<Outcome> {
    let _guard = CtxGuard::enter(ctx);
    let cfg = load_config(app)?;
    // Budget for compaction and for the outgoing hard trim. A local server's
    // context window is far smaller than a cloud model's, so it gets a tighter
    // one.
    let cap = compact_bytes(&cfg);
    // Builtins are stable for the run. MCP tools are re-listed whenever the
    // mcp config changes (the model can add servers mid-run with
    // add_mcp_server). App actions are re-read each iteration because the
    // page re-registers them after every reload.
    let builtins = builtin_tools(app, ctx);
    let mut mcp_snapshot = String::new();
    let mut mcp_cached: Vec<Value> = Vec::new();
    let mut steps = 0usize;
    let mut report = String::new();

    for _ in 0..max_iters {
        if ctx.stopped() {
            break;
        }
        // Every iteration, not just at run start: a long tool-using run grows
        // the history by tens of KB per step and would otherwise sail past the
        // model's context window mid-run.
        auto_compact(app, ctx, cap);
        let system = system_prompt(app, ctx); // refreshed: schema/files change as we work
        let mcp_now = serde_json::to_string(&rpc::mcp_servers_enabled(app)).unwrap_or_default();
        if mcp_now != mcp_snapshot {
            mcp_cached = mcp_tools(app);
            mcp_snapshot = mcp_now;
        }
        let mut tools = builtins.clone();
        tools.extend(mcp_cached.clone());
        tools.extend(app_action_tools(app));
        // Captured BEFORE building: any user row landing after this id was not
        // part of this turn's context and still needs an answer.
        let seen_id = last_chat_id(app, &ctx.session);
        let msgs = build_messages(app, &ctx.session, cap)?;
        if msgs.is_empty() {
            break;
        }
        let blocks = call_provider_retrying(app, ctx, &cfg, &system, &msgs, &tools)?;
        steps += 1;
        let tool_uses: Vec<Value> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .cloned()
            .collect();
        let text: String = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(|t| t.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            report = text;
        }
        let mid = format!("a-{}-{}", crate::store::now_ms(), fastrand::u32(..));
        rpc::local_op(app, "chat",
            json!({"mid": mid, "role": "assistant", "session": ctx.session,
                   "content": {"blocks": blocks}}))?;
        if tool_uses.is_empty() {
            // A message sent while this turn was in flight (this device or
            // another) would otherwise sit unanswered: chat.send sees the run
            // as live and doesn't start a second one.
            if has_user_after(app, &ctx.session, seen_id) {
                continue;
            }
            break;
        }
        let mut results = Vec::new();
        for tu in &tool_uses {
            if ctx.stopped() {
                results.push(json!({"id": tu["id"], "name": tu["name"],
                    "content": "(stopped by user)", "is_error": true}));
                continue;
            }
            let name = tu["name"].as_str().unwrap_or("");
            let id = tu["id"].as_str().unwrap_or("");
            // Plan mode backstop: write tools are absent from the offered
            // toolset, but a hallucinated call must not execute either.
            if plan_mode(app)
                && (crate::tools::is_gated(app, name) || crate::tools::is_agent_tool(name))
            {
                results.push(json!({"id": tu["id"], "name": tu["name"],
                    "content": "Plan mode is read-only — this tool is unavailable. \
                                Propose the change; the user will switch modes to apply it.",
                    "is_error": true}));
                continue;
            }
            let forced = crate::tools::forced_gate(name, &tu["input"]);
            if !approval_gate(app, ctx, id, name, &tu["input"], &forced) {
                // Denied: a normal (non-crashing) result the model can adapt to.
                results.push(json!({"id": tu["id"], "name": tu["name"],
                    "content": format!("The user denied permission to run {name}."),
                    "is_error": true, "denied": true}));
                continue;
            }
            app.notify("ai", json!({"state": "tool", "tool": name, "session": ctx.session}));
            let mut input = tu["input"].clone();
            if forced.is_some() {
                // Marks the user's approval for the RPC layer; stripped from
                // anything arriving over the wire, so only this path sets it.
                input["_user_approved"] = json!(true);
            }
            let r = if let Some(bad) = input["_invalid"].as_str() {
                Err(anyhow!(
                    "the arguments of this tool call did not arrive as valid JSON (the response was \
                     probably cut off) — the tool did NOT run. Send the call again. Received: {}",
                    bad.chars().take(200).collect::<String>()
                ))
            } else {
                run_tool(app, name, &input)
            };
            let (mut content, is_error) = match r {
                Ok(v) => (v.to_string(), false),
                // A schema-shaped failure (bad/missing params) is fed back WITH
                // the tool's schema so the model can repair the call itself
                // instead of guessing the same shape again.
                Err(e) => (format!("Error: {e}{}", schema_hint(name, &e)), true),
            };
            // Cap results AND errors: an MCP server's error text can be
            // arbitrarily large and would otherwise ride along in every later
            // request of the conversation.
            if content.len() > TOOL_RESULT_LIMIT {
                let was = content.len();
                // char-boundary-safe: a plain truncate panics mid-UTF-8
                crate::rpc::truncate_utf8(&mut content, TOOL_RESULT_LIMIT);
                content.push_str(&format!(
                    "…(truncated — output was {}KB. Re-run with narrower parameters \
                     (LIMIT, column lists, byte/row ranges) rather than requesting it all.)",
                    was.div_ceil(1024)
                ));
            }
            results.push(json!({"id": tu["id"], "name": tu["name"],
                "content": content, "is_error": is_error}));
        }
        // Auto-surface (but don't dump): if the live app has logged new
        // errors/warnings since the AI last looked — e.g. the edit just made
        // reloaded the iframe and it threw — staple a one-line nudge to the
        // final result so the model knows to call read_console.
        if let Some(note) = app.console_alert() {
            if let Some(last) = results.last_mut() {
                if let Some(c) = last["content"].as_str() {
                    last["content"] = json!(format!("{c}{note}"));
                }
            }
        }
        let mid = format!("t-{}-{}", crate::store::now_ms(), fastrand::u32(..));
        rpc::local_op(app, "chat",
            json!({"mid": mid, "role": "tool", "session": ctx.session,
                   "content": {"results": results}}))?;
    }
    Ok(Outcome { steps, report })
}

/// Newest chat row id in a session — the watermark for "seen by this turn".
fn last_chat_id(app: &Arc<App>, session: &str) -> i64 {
    let eng = app.engine.lock().unwrap();
    store::query(
        &eng.db,
        "SELECT COALESCE(max(id), 0) FROM uapp_chat WHERE COALESCE(session,'main') = ?1",
        &[json!(session)],
    )
    .map(|r| r["rows"][0][0].as_i64().unwrap_or(0))
    .unwrap_or(0)
}

/// Did a user message land after the watermark? (Late sends into a session
/// whose loop was about to finish — including ones that arrived mid-provider
/// call, which sort BEFORE the assistant reply.)
fn has_user_after(app: &Arc<App>, session: &str, seen_id: i64) -> bool {
    let eng = app.engine.lock().unwrap();
    store::query(
        &eng.db,
        "SELECT count(*) FROM uapp_chat WHERE COALESCE(session,'main') = ?1 AND role = 'user'
         AND id > ?2",
        &[json!(session), json!(seen_id)],
    )
    .map(|r| r["rows"][0][0].as_i64().unwrap_or(0) > 0)
    .unwrap_or(false)
}

/// Is the newest row of this session a user message nobody answered yet?
pub fn unanswered(app: &Arc<App>, session: &str) -> bool {
    let eng = app.engine.lock().unwrap();
    store::query(
        &eng.db,
        "SELECT role FROM uapp_chat WHERE COALESCE(session,'main') = ?1 ORDER BY id DESC LIMIT 1",
        &[json!(session)],
    )
    .map(|r| r["rows"][0][0].as_str() == Some("user"))
    .unwrap_or(false)
}

/// For a failure that looks like a bad tool input, append the tool's JSON
/// schema so the next attempt can be well-formed.
fn schema_hint(name: &str, err: &anyhow::Error) -> String {
    let msg = err.to_string().to_lowercase();
    let schema_shaped = ["missing required", "must be", "expected", "invalid", "not an object",
                         "unknown field", "needs "]
        .iter()
        .any(|k| msg.contains(k));
    if !schema_shaped {
        return String::new();
    }
    match crate::tools::registry().iter().find(|t| t.name == name) {
        Some(def) => format!(
            "\nInvalid input for {name}. Expected schema: {}",
            (def.schema)()
        ),
        None => String::new(),
    }
}

// ---- compaction ---------------------------------------------------------------

/// Rows of one session, with the (ts, ctr) ordering key compaction needs.
fn session_rows(app: &Arc<App>, session: &str) -> Result<Vec<Value>> {
    let eng = app.engine.lock().unwrap();
    let r = store::query(
        &eng.db,
        "SELECT mid, ts, ctr, role, content, device FROM uapp_chat
         WHERE COALESCE(session,'main') = ?1 ORDER BY ts, ctr, device, id",
        &[json!(session)],
    )?;
    Ok(r["rows"].as_array().cloned().unwrap_or_default())
}

/// A readable transcript of chat rows for the summarizer (newest kept when the
/// cap bites — the tail is the part that matters most).
fn transcript(rows: &[Value], cap: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    for row in rows {
        let role = row[3].as_str().unwrap_or("");
        let content: Value =
            serde_json::from_str(row[4].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        let body = match role {
            "user" | "system" => content["text"].as_str().unwrap_or("").to_string(),
            "summary" => format!("(earlier summary) {}", content["text"].as_str().unwrap_or("")),
            "assistant" => content["blocks"]
                .as_array()
                .map(|bs| {
                    bs.iter()
                        .filter_map(|b| match b["type"].as_str().unwrap_or("") {
                            "text" => b["text"].as_str().map(|t| t.to_string()),
                            "tool_use" => Some(format!(
                                "(called {} {})",
                                b["name"].as_str().unwrap_or(""),
                                clip(&b["input"].to_string(), 400)
                            )),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            "tool" => content["results"]
                .as_array()
                .map(|rs| {
                    rs.iter()
                        .map(|r| {
                            format!(
                                "(result {}) {}",
                                r["name"].as_str().unwrap_or(""),
                                clip(r["content"].as_str().unwrap_or(""), 1500)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        if !body.trim().is_empty() {
            parts.push(format!("{role}: {body}"));
        }
    }
    let mut out = parts.join("\n\n");
    if out.len() > cap {
        let mut start = out.len() - cap;
        while !out.is_char_boundary(start) {
            start += 1;
        }
        out = format!("…(earlier turns omitted)\n{}", &out[start..]);
    }
    out
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Summarize everything in a session except the last two user↔assistant
/// exchanges, then replace it with one `summary` row. The summary text is
/// produced HERE, before the op is emitted, so replay is deterministic.
pub fn compact_session(app: &Arc<App>, session: &str) -> Result<Value> {
    let cfg = load_config(app)?;
    let rows = session_rows(app, session)?;
    let user_at: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r[3].as_str() == Some("user"))
        .map(|(i, _)| i)
        .collect();
    // Keep the last two exchanges; nothing older means nothing to do.
    let keep_from = if user_at.len() >= 2 { user_at[user_at.len() - 2] } else { 0 };
    if keep_from == 0 {
        return Ok(json!({"ok": true, "kept": rows.len(), "summarized_bytes": 0,
                         "note": "nothing old enough to compact"}));
    }
    let older = &rows[..keep_from];
    let summarized_bytes: usize =
        older.iter().map(|r| r[4].as_str().map(|s| s.len()).unwrap_or(0)).sum();
    let text = transcript(older, 300_000);
    if text.trim().is_empty() {
        return Ok(json!({"ok": true, "kept": rows.len(), "summarized_bytes": 0,
                         "note": "nothing to summarize"}));
    }
    let msgs = vec![("user".to_string(), vec![json!({"type": "text", "text": text})])];
    // Sentinel session: the summarizer's streamed deltas must not render as a
    // live draft bubble in the chat being compacted (no client shows this id).
    let blocks =
        call_provider(app, &format!("compact:{session}"), &cfg, COMPACT_PROMPT, &msgs, &[])?;
    let summary: String = blocks
        .iter()
        .filter_map(|b| b["text"].as_str().map(|t| t.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if summary.is_empty() {
        bail!("the summarizer returned no text — nothing was compacted");
    }
    let boundary = &rows[keep_from - 1];
    rpc::local_op(app, "chat_compact", json!({
        "session": session,
        "upto_ts": boundary[1],
        "upto_ctr": boundary[2],
        // Tie-breaker matching the sort order: a concurrent row from another
        // device with the same (ts,ctr) that sorts AFTER the boundary is part
        // of the kept tail and must not be deleted.
        "upto_dev": boundary[5],
        "mid": format!("sum-{}-{}", crate::store::now_ms(), fastrand::u32(..)),
        "summary": summary,
    }))?;
    Ok(json!({"ok": true, "kept": rows.len() - keep_from, "summarized_bytes": summarized_bytes}))
}

/// Compact automatically when a run starts on an oversized history. Guarded
/// against looping: if the oldest row is ALREADY a summary, compacting again
/// would just re-summarize a summary — the outgoing hard trim handles it.
fn auto_compact(app: &Arc<App>, ctx: &RunCtx, cap: usize) {
    let Ok(rows) = session_rows(app, &ctx.session) else { return };
    let bytes: usize = rows.iter().map(|r| r[4].as_str().map(|s| s.len()).unwrap_or(0)).sum();
    if bytes <= cap {
        return;
    }
    if rows.first().map(|r| r[3].as_str() == Some("summary")).unwrap_or(false) {
        return;
    }
    app.notify("ai", json!({"state": "running", "session": ctx.session,
                            "note": "summarizing the conversation so far…"}));
    if let Err(e) = compact_session(app, &ctx.session) {
        eprintln!("uapp: auto-compaction failed: {e}");
    }
}

// ---- sub-agents ---------------------------------------------------------------

/// Delegate a self-contained task to a sub-agent: its own chat session, the
/// same tools (minus the agent tools), the same approval gate, run
/// synchronously on this thread so the calling loop simply waits for a report.
pub fn agent_run(app: &Arc<App>, input: &Value) -> Result<Value> {
    let ctx = current_ctx();
    if ctx.depth > 0 {
        bail!("sub-agents cannot start sub-agents");
    }
    let description = input["description"].as_str().unwrap_or("").trim();
    let description = if description.is_empty() { "agent task" } else { description };
    let prompt = input["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("agent_run needs a non-empty prompt"))?;
    let mode = match input["mode"].as_str().unwrap_or("fresh") {
        "fork" => "fork",
        "fresh" => "fresh",
        other => bail!("mode must be \"fresh\" or \"fork\" (got {other})"),
    };
    let max_steps = input["max_steps"].as_u64().unwrap_or(12).clamp(1, 24) as usize;
    let session = format!("ag-{}-{}", crate::store::now_ms(), fastrand::u32(..));
    rpc::local_op(app, "session_meta", json!({
        "id": session, "title": clip(description, 200), "kind": "agent",
        "parent": ctx.session, "created": crate::store::now_ms(),
    }))?;
    let seed = if mode == "fork" {
        let digest = match build_messages(app, &ctx.session, AUTO_COMPACT_BYTES) {
            Ok(msgs) => clip(&json!(msgs).to_string(), FORK_DIGEST_BYTES),
            Err(_) => String::new(),
        };
        format!("Context from the conversation you are helping with (JSON digest):\n{digest}\n\nYour task:\n{prompt}")
    } else {
        prompt.to_string()
    };
    rpc::local_op(app, "chat", json!({
        "mid": format!("{}-{}", crate::store::now_ms(), fastrand::u32(..)),
        "role": "user", "session": session,
        "content": {"text": seed, "attachments": []},
    }))?;
    agent_loop(app, &ctx, session, max_steps)
}

/// Continue an existing sub-agent conversation with another instruction.
pub fn agent_send(app: &Arc<App>, input: &Value) -> Result<Value> {
    let ctx = current_ctx();
    if ctx.depth > 0 {
        bail!("sub-agents cannot drive other sub-agents");
    }
    let session = rpc::session_id(
        input["session"].as_str().ok_or_else(|| anyhow!("agent_send needs a session"))?,
    )?;
    let message = input["message"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("agent_send needs a non-empty message"))?;
    let kind: Option<String> = {
        let eng = app.engine.lock().unwrap();
        let r = store::query(
            &eng.db,
            "SELECT kind FROM uapp_sessions WHERE id = ?1",
            &[json!(session)],
        )?;
        r["rows"][0][0].as_str().map(|s| s.to_string())
    };
    match kind.as_deref() {
        Some("agent") => {}
        Some(other) => bail!("session {session} is a '{other}' chat, not an agent conversation"),
        None => bail!("no agent session named {session}"),
    }
    rpc::local_op(app, "chat", json!({
        "mid": format!("{}-{}", crate::store::now_ms(), fastrand::u32(..)),
        "role": "user", "session": session,
        "content": {"text": message, "attachments": []},
    }))?;
    agent_loop(app, &ctx, session, 12)
}

/// Run one sub-agent loop to completion and shape its tool result.
fn agent_loop(
    app: &Arc<App>,
    parent: &RunCtx,
    session: String,
    max_steps: usize,
) -> Result<Value> {
    let child = parent.child(session.clone());
    let out = run_loop(app, &child, max_steps)?;
    let report = if out.report.trim().is_empty() {
        "(the sub-agent produced no final text — open its conversation to see what it did)"
            .to_string()
    } else {
        out.report
    };
    Ok(json!({"session": session, "steps": out.steps, "report": report}))
}
