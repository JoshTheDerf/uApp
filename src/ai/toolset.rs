//! The tools a run offers the model, and running one by name.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;

use super::{plan_mode, RunCtx};

/// Builtin tools from the registry (see tools.rs), minus disabled ones — and
/// minus the agent tools inside a sub-agent loop, which is what bounds
/// recursion to depth 1.
pub(crate) fn builtin_tools(app: &Arc<App>, ctx: &RunCtx) -> Vec<Value> {
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

pub(crate) fn mcp_tools(app: &Arc<App>) -> Vec<Value> {
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
pub(crate) fn app_action_tools(app: &Arc<App>) -> Vec<Value> {
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
