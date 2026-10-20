//! The tools a run offers the model, and running one by name.
//!
//! A run fixes its tool list when it starts ([`Toolset::freeze`]). Tools are
//! the first thing in a request, so a list that changed between steps would
//! throw away the prompt cache and, for Claude, the thinking carried from
//! earlier steps. App actions registered later in the run are still
//! reachable through the `call_action` tool.
//!
//! Every offered name matches `^[a-zA-Z0-9_-]{1,64}$`, the rule providers
//! enforce (one bad name fails every request). MCP and app action names are
//! made to fit, and the toolset keeps the map from offered name to what it
//! runs, so a server name containing `__` can't be misread either.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;

use super::RunCtx;

/// What an offered tool name runs.
#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// A tools.rs registry tool.
    Builtin(String),
    /// A tool on an MCP server (the server's config entry).
    Mcp { server: Value, tool: String },
    /// An action the app page registered.
    App(String),
}

impl Target {
    /// The tool's canonical name: what approvals show, what the disabled
    /// list stores and what apps pass to `uapp.tool`.
    pub(crate) fn raw_name(&self) -> String {
        match self {
            Target::Builtin(n) => n.clone(),
            Target::Mcp { server, tool } => {
                format!("mcp__{}__{tool}", server["name"].as_str().unwrap_or(""))
            }
            Target::App(a) => format!("app__{a}"),
        }
    }
}

/// The longest tool name every provider accepts.
pub(crate) const MAX_TOOL_NAME: usize = 64;

/// Make `raw` a valid tool name, unique among `taken`. Characters outside
/// `[a-zA-Z0-9_-]` become `_`; a name that is too long or clashes keeps a
/// readable head and gets a short hash of the original, so the same raw
/// name maps to the same safe name every time.
pub(crate) fn safe_tool_name(raw: &str, taken: &HashSet<String>) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = if s.is_empty() { "tool".to_string() } else { s };
    if s.len() <= MAX_TOOL_NAME && !taken.contains(&s) {
        return s;
    }
    // FNV-1a: stable across runs and builds, unlike the std hasher.
    let hash = raw.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    }) as u32;
    for n in 0u32.. {
        let suffix = if n == 0 {
            format!("_{hash:08x}")
        } else {
            format!("_{hash:08x}{n}")
        };
        // `s` is ASCII, so any byte index is a char boundary.
        let head = &s[..s.len().min(MAX_TOOL_NAME - suffix.len())];
        let candidate = format!("{head}{suffix}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// One run's tools: what is offered, and what each offered name runs.
pub(crate) struct Toolset {
    pub tools: Vec<Value>,
    pub targets: HashMap<String, Target>,
    /// What the list was built from; a different key means rebuild.
    pub key: String,
}

/// The inputs a toolset depends on that can change mid-run by an explicit
/// choice: plan mode, the MCP servers, the tools switched off.
pub(crate) fn toolset_key(app: &Arc<App>, plan: bool) -> String {
    let mut disabled: Vec<String> = crate::tools::disabled_set(app).into_iter().collect();
    disabled.sort();
    json!([plan, rpc::mcp_servers_enabled(app), disabled]).to_string()
}

impl Toolset {
    /// Builtins from the registry, then MCP tools, then the app's actions,
    /// minus disabled ones. A sub-agent gets no agent tools (which bounds
    /// recursion to depth 1). Plan mode is a read-only toolset: gated tools
    /// can write, agents can write inside their own loop, and MCP side
    /// effects are unknown, so all three are withheld.
    pub(crate) fn freeze(app: &Arc<App>, ctx: &RunCtx, plan: bool) -> Toolset {
        let key = toolset_key(app, plan);
        let disabled = crate::tools::disabled_set(app);
        let mut taken = HashSet::new();
        let mut tools = Vec::new();
        let mut targets = HashMap::new();
        let mut offer = |target: Target, description: String, schema: Value| {
            let raw = target.raw_name();
            if disabled.contains(&raw) {
                return;
            }
            let name = safe_tool_name(&raw, &taken);
            taken.insert(name.clone());
            tools.push(json!({"name": name, "description": description, "input_schema": schema}));
            targets.insert(name, target);
        };
        for t in crate::tools::registry() {
            let agent = crate::tools::is_agent_tool(t.name);
            if (ctx.depth > 0 && agent) || (plan && (t.gated || agent)) {
                continue;
            }
            offer(
                Target::Builtin(t.name.to_string()),
                t.description.to_string(),
                (t.schema)(),
            );
        }
        if !plan {
            let servers = rpc::mcp_servers_enabled(app);
            for t in crate::mcp::list_all_tools(&servers) {
                let server_name = t["server"].as_str().unwrap_or("");
                let Some(server) = servers.iter().find(|s| s["name"] == server_name) else {
                    continue;
                };
                offer(
                    Target::Mcp {
                        server: server.clone(),
                        tool: t["name"].as_str().unwrap_or("").to_string(),
                    },
                    format!(
                        "[{server_name} MCP] {}",
                        t["description"].as_str().unwrap_or("")
                    ),
                    t["input_schema"].clone(),
                );
            }
        }
        // The page re-registers its actions after every reload, so this is
        // the set as of the run's start; later ones go through call_action.
        for (name, a) in app.actions_snapshot() {
            if plan && !a.readonly {
                continue;
            }
            offer(
                Target::App(name),
                format!(
                    "[app action{}] {}",
                    if a.readonly { ", read-only" } else { "" },
                    a.description
                ),
                a.schema,
            );
        }
        Toolset {
            tools,
            targets,
            key,
        }
    }

    /// What a name the model called runs. Unknown names fall back to the
    /// canonical forms (`app__x`, `mcp__server__tool`, builtins), which a
    /// model may produce for a tool it learned about mid-run.
    pub(crate) fn resolve(&self, app: &Arc<App>, name: &str) -> Result<Target> {
        if let Some(t) = self.targets.get(name) {
            return Ok(t.clone());
        }
        resolve_raw(app, name)
    }
}

/// A canonical tool name to its target.
pub(crate) fn resolve_raw(app: &Arc<App>, name: &str) -> Result<Target> {
    if let Some(action) = name.strip_prefix("app__") {
        return Ok(Target::App(action.to_string()));
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        // The longest configured server name that fits, so a server named
        // "a__b" isn't read as server "a" with tool "b__…".
        let servers = rpc::mcp_servers_enabled(app);
        let found = servers
            .iter()
            .filter_map(|s| {
                let server = s["name"].as_str()?;
                let tool = rest.strip_prefix(server)?.strip_prefix("__")?;
                Some((server.len(), s, tool))
            })
            .max_by_key(|(len, _, _)| *len);
        return match found {
            Some((_, server, tool)) => Ok(Target::Mcp {
                server: server.clone(),
                tool: tool.to_string(),
            }),
            None => bail!("no enabled MCP server matches the tool name {name}"),
        };
    }
    if crate::tools::registry().iter().any(|t| t.name == name) {
        return Ok(Target::Builtin(name.to_string()));
    }
    bail!("unknown tool {name}")
}

/// `call_action {action, input}` is the action itself, for gating and
/// running alike. Returns the target and the input to pass it.
pub(crate) fn unwrap_call_action(target: Target, input: &Value) -> (Target, Value) {
    match &target {
        Target::Builtin(n) if n == "call_action" => (
            Target::App(input["action"].as_str().unwrap_or("").to_string()),
            input
                .get("input")
                .cloned()
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| json!({})),
        ),
        _ => (target, input.clone()),
    }
}

/// The app's registered actions, for a model that called one that isn't.
pub(crate) fn action_list(app: &Arc<App>) -> Value {
    json!(app
        .actions_snapshot()
        .into_iter()
        .map(
            |(name, a)| json!({"action": name, "description": a.description,
                                "readonly": a.readonly, "params": a.schema})
        )
        .collect::<Vec<_>>())
}

pub(crate) fn run_target(app: &Arc<App>, target: &Target, input: &Value) -> Result<Value> {
    match target {
        Target::App(action) => {
            if action.is_empty() {
                bail!(
                    "call_action needs an action name. Registered actions: {}",
                    action_list(app)
                );
            }
            app.invoke_action(action, input).map_err(|e| {
                if e.to_string().starts_with("no app action named") {
                    anyhow!("{e}\nRegistered actions: {}", action_list(app))
                } else {
                    e
                }
            })
        }
        Target::Mcp { server, tool } => crate::mcp::call_tool(server, tool, input.clone()),
        Target::Builtin(name) => match crate::tools::registry().iter().find(|t| t.name == name) {
            Some(def) => (def.run)(app, input),
            None => bail!("unknown tool {name}"),
        },
    }
}

/// Run a tool by its canonical name: the path for apps (`uapp.tool`) and
/// the shell's tools.call.
pub fn run_tool(app: &Arc<App>, name: &str, input: &Value) -> Result<Value> {
    let target = resolve_raw(app, name)?;
    run_target(app, &target, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(n: &str) -> bool {
        !n.is_empty()
            && n.len() <= MAX_TOOL_NAME
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    #[test]
    fn names_are_made_valid_unique_and_stable() {
        let mut taken = HashSet::new();
        let plain = safe_tool_name("mcp__crm__echo", &taken);
        assert_eq!(plain, "mcp__crm__echo");
        taken.insert(plain);
        let unicode = safe_tool_name("mcp__café__résumé.get", &taken);
        assert!(valid(&unicode), "{unicode}");
        let long_raw = format!("app__{}", "x".repeat(80));
        let long = safe_tool_name(&long_raw, &taken);
        assert!(valid(&long) && long.len() == MAX_TOOL_NAME, "{long}");
        assert_eq!(
            long,
            safe_tool_name(&long_raw, &taken),
            "same input, same name"
        );
        // Two raw names that sanitize alike don't collide.
        taken.insert(safe_tool_name("mcp__a__b.c", &taken));
        let second = safe_tool_name("mcp__a__b/c", &taken);
        assert!(valid(&second) && !taken.contains(&second));
        assert_eq!(safe_tool_name("", &taken), "tool");
    }

    #[test]
    fn call_action_unwraps_to_the_action() {
        let (t, input) = unwrap_call_action(
            Target::Builtin("call_action".into()),
            &json!({"action": "add_note", "input": {"text": "hi"}}),
        );
        assert_eq!(t.raw_name(), "app__add_note");
        assert_eq!(input, json!({"text": "hi"}));
        let (t, _) = unwrap_call_action(Target::Builtin("sql_query".into()), &json!({}));
        assert_eq!(t.raw_name(), "sql_query");
    }
}
