//! The assistant's system prompt (text in system_prompt.txt) and the app
//! snapshot that travels in the conversation instead.
//!
//! The system prompt is built once per run and holds nothing that changes
//! while the assistant works: it is the first thing in every request, so any
//! change there would throw away the prompt cache for the whole conversation.
//! The app's files and schema, which do change, go into the chat as
//! `context` rows (see run.rs), appended only when they differ.

use std::sync::Arc;

use super::RunCtx;
use crate::app::App;
use crate::store;

pub(crate) const SUBAGENT_SUFFIX: &str = "\n\nYou are an autonomous sub-agent. Complete the task fully and return a clear, self-contained final report as your final message. You cannot ask the user questions.";

const PLAN_SUFFIX: &str = "\n\nYou are in PLAN MODE (read-only). Do NOT modify data, files or config — \
     write tools are unavailable. Investigate with the read tools and propose a \
     concrete plan; the user will switch modes before you apply it.";

const TEMPLATE: &str = include_str!("system_prompt.txt");

/// Replace each `{key}` in one pass, so a value that happens to contain
/// another placeholder is never expanded.
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

pub(crate) fn system_prompt(app: &Arc<App>, ctx: &RunCtx, plan: bool) -> String {
    let name = {
        let eng = app.eng();
        store::meta_get(&eng.db, "name")
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    fill(TEMPLATE, &[("name", &name)])
        + if ctx.depth > 0 { SUBAGENT_SUFFIX } else { "" }
        + if plan { PLAN_SUFFIX } else { "" }
}

/// Most file lines and schema bytes in one snapshot; past that the model is
/// pointed at list_files / get_schema.
const SNAPSHOT_FILES: usize = 300;
const SNAPSHOT_SCHEMA_BYTES: usize = 16 * 1024;

/// The app's files and data schema as a note for the conversation.
pub(crate) fn app_snapshot(app: &Arc<App>) -> String {
    let (files, schema) = {
        let eng = app.eng();
        let files = store::query(&eng.db, "SELECT name, sz FROM sqlar ORDER BY name", &[])
            .map(|v| v["rows"].as_array().cloned().unwrap_or_default())
            .unwrap_or_default();
        let schema = store::query(
            &eng.db,
            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'uapp_%' \
             AND name NOT LIKE 'sqlite_%' AND name NOT IN ('sqlar') ORDER BY type DESC, name",
            &[],
        )
        .map(|v| v["rows"].as_array().cloned().unwrap_or_default())
        .unwrap_or_default();
        (files, schema)
    };
    let mut out = format!("[App state]\nFiles ({}):\n", files.len());
    for f in files.iter().take(SNAPSHOT_FILES) {
        let size = f[1].as_i64().unwrap_or(0);
        out.push_str(&format!(
            "{} ({})\n",
            f[0].as_str().unwrap_or(""),
            if size < 1024 {
                format!("{size} B")
            } else {
                format!("{:.1} KB", size as f64 / 1024.0)
            }
        ));
    }
    if files.len() > SNAPSHOT_FILES {
        out.push_str(&format!(
            "…and {} more (list_files shows them all)\n",
            files.len() - SNAPSHOT_FILES
        ));
    }
    out.push_str("Data schema:\n");
    let mut sql = String::new();
    for r in &schema {
        sql.push_str(r[0].as_str().unwrap_or(""));
        sql.push_str(";\n");
    }
    if sql.is_empty() {
        sql.push_str("(no tables yet)\n");
    }
    if sql.len() > SNAPSHOT_SCHEMA_BYTES {
        crate::rpc::truncate_utf8(&mut sql, SNAPSHOT_SCHEMA_BYTES);
        sql.push_str("\n…(cut off; get_schema shows all of it)\n");
    }
    out.push_str(&sql);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_expands_each_placeholder_once() {
        assert_eq!(
            fill("a {name} b {x} {name}", &[("name", "{name}")]),
            "a {name} b {x} {name}"
        );
    }

    #[test]
    fn template_has_no_volatile_parts() {
        // The prompt must not change while a run works: nothing about files
        // or schema in it, only the app's name.
        assert!(!TEMPLATE.contains("{files}") && !TEMPLATE.contains("{schema}"));
        let rest = fill(TEMPLATE, &[("name", "")]);
        assert!(!rest.contains("{name}"));
    }
}
