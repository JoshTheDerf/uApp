//! The assistant's system prompt. The text lives in system_prompt.txt;
//! `{name}`, `{files}` and `{schema}` are filled in here.

use std::sync::Arc;

use super::{plan_mode, RunCtx};
use crate::app::App;
use crate::store;

pub(crate) const SUBAGENT_SUFFIX: &str = "\n\nYou are an autonomous sub-agent. Complete the task fully and return a clear, self-contained final report as your final message. You cannot ask the user questions.";

const TEMPLATE: &str = include_str!("system_prompt.txt");

/// Replace each `{key}` in one pass, so a value that happens to contain
/// another placeholder (an app named "{files}") is never expanded.
fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    'scan: while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        for (k, v) in vars {
            let key = format!("{{{k}}}");
            if rest.starts_with(&key) {
                out.push_str(v);
                rest = &rest[key.len()..];
                continue 'scan;
            }
        }
        out.push('{');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn system_prompt(app: &Arc<App>, ctx: &RunCtx) -> String {
    let (files, schema, name) = {
        let eng = app.eng();
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
        let name = store::meta_get(&eng.db, "name")
            .ok()
            .flatten()
            .unwrap_or_default();
        (files, schema, name)
    };
    fill(
        TEMPLATE,
        &[("name", &name), ("files", &files), ("schema", &schema)],
    ) + if ctx.depth > 0 { SUBAGENT_SUFFIX } else { "" }
        + if plan_mode(app) {
            "\n\nYou are in PLAN MODE (read-only). Do NOT modify data, files or config — \
             write tools are unavailable. Investigate with the read tools and propose a \
             concrete plan; the user will switch modes before you apply it."
        } else {
            ""
        }
}
