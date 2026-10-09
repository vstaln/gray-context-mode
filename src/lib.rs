//! `gray_context_mode` — the context-mode plugin core.
//!
//! Two tools over one SQLite FTS5 store:
//! - `ctx_exec` runs a shell command in a sandboxed scratch dir (bwrap when
//!   available, cwd-restricted otherwise), indexes every byte of
//!   stdout/stderr into the KB, and returns only a summary + the run label
//!   (`exec:N`) to query later.
//! - `ctx_search` runs an FTS5 query over captured output and returns just
//!   the matching lines, bounded.
//!
//! The point: builds, greps, test runs and logs whose output would flood
//! the context window go through ctx_exec; the agent pulls back only the
//! slice it needs with ctx_search.

pub mod exec;
pub mod store;

use serde_json::{json, Value};
use std::io::BufReader;
use std::path::PathBuf;

/// Plugin name claimed in `plugin/manifest`.
pub const PLUGIN_NAME: &str = "context-mode";
/// Protocol version claimed in `plugin/manifest`.
pub const PROTOCOL: &str = "1.1";

/// Search bounds — keep responses small even on huge indexes.
const DEFAULT_LIMIT: u32 = 8;
const MAX_LIMIT: u32 = 50;
const DEFAULT_CONTEXT: u32 = 1;
const MAX_CONTEXT: u32 = 5;
/// Per-line display cap.
const LINE_CAP: usize = 240;
/// Total response cap across all queries/hits.
const RESPONSE_CAP: usize = 16 * 1024;

fn exec_tool_def() -> Value {
    json!({
        "name": "ctx_exec",
        "label": "Ctx Exec",
        "description": "Run a shell command in a sandboxed scratch dir and index ALL stdout/stderr into a searchable store — no command output is echoed back. Returns a summary only: exit code, line/byte counts, and a run label like \"exec:7\". Retrieve slices afterwards with ctx_search(query: \"...\", source: \"exec:7\"). Use this for builds, tests, greps and logs whose output would flood context; prefer it over a direct shell call whenever output could exceed ~100 lines.",
        "parameters": {
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command run via bash -c."},
                "cwd": {"type": "string", "description": "Working directory, bound read-write into the sandbox. Default: the calling session's cwd (so builds/tests run against the project); a fresh scratch dir when no session cwd is known."},
                "timeout_ms": {"type": "integer", "description": "Wall-clock limit in ms. Default 120000, max 600000. Kills the whole process group on expiry."},
                "label": {"type": "string", "description": "Optional name appended to the run label (exec:N:<label>) for later source filtering."}
            },
            "required": ["command"],
            "additionalProperties": false
        }
    })
}

fn search_tool_def() -> Value {
    json!({
        "name": "ctx_search",
        "label": "Ctx Search",
        "description": "Full-text (FTS5) query over output captured by ctx_exec. Returns only the matching lines with run label, stream and line numbers — bounded, never the whole output. Args: query or queries (array runs each in one call), source to scope to a run (\"exec:7\" or a label substring), limit per query (default 8, max 50), context lines around each match (default 1, max 5). Plain words are fine — FTS5 operators AND/OR/NOT/\"phrase\"/prefix* also work.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "FTS5 query. Prefer 2-4 specific terms."},
                "queries": {"type": "array", "items": {"type": "string"}, "description": "Multiple queries in one call — cheaper than separate calls."},
                "source": {"type": "string", "description": "Scope to one run: \"exec:N\" or a label substring. Omit to search all captured output."},
                "limit": {"type": "integer", "description": "Max matching chunks per query (default 8, max 50)."},
                "context": {"type": "integer", "description": "Unmatched lines to include around each match (default 1, max 5)."},
                "stream": {"type": "string", "enum": ["stdout", "stderr"], "description": "Only return lines from this stream."}
            },
            "additionalProperties": false
        }
    })
}

/// The `plugin/manifest` result — also printed by `gray-context-mode
/// manifest` for install registration.
pub fn manifest() -> Value {
    json!({
        "name": PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "sidecar_args": ["sidecar"],
        "tools": [exec_tool_def(), search_tool_def()],
        "commands": [],
        "hooks": ["prompt/context"],
        "completion": ["exec", "search", "stats", "clear"],
    })
}

/// `prompt/context` guidance for the model.
pub fn prompt_guidance() -> &'static str {
    "ctx_exec + ctx_search keep huge command output out of context: ctx_exec runs a shell command \
     sandboxed, indexes stdout+stderr into a searchable store, and returns only a summary \
     (exit code, counts, run label exec:N) — never the output itself. Then ctx_search(query, \
     source:\"exec:N\") returns just the matching lines. Prefer ctx_exec over plain bash for \
     builds, tests, greps, and logs that could exceed ~100 lines; run the quiet command \
     normally when you need its output verbatim."
}

/// The calling session's cwd hint, else the process cwd.
fn session_cwd(params: &Value) -> Option<PathBuf> {
    params
        .pointer("/session/cwd")
        .and_then(Value::as_str)
        .or_else(|| params.get("cwd").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

fn human_bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn tool_error(msg: impl std::fmt::Display) -> Value {
    json!({ "content": format!("{msg}"), "is_error": true })
}

/// `ctx_exec` handler: sandboxed run → index both streams → summary only.
fn ctx_exec(db: &std::path::Path, params: &Value) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return tool_error("ctx_exec requires a 'command' string");
    };
    if command.trim().is_empty() {
        return tool_error("ctx_exec: 'command' is empty");
    }
    let cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| session_cwd(params));
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(exec::DEFAULT_TIMEOUT_MS)
        .min(exec::MAX_TIMEOUT_MS);
    let label = args.get("label").and_then(Value::as_str);

    let result = match exec::run(command, cwd.as_deref(), timeout_ms) {
        Ok(r) => r,
        Err(e) => return tool_error(format!("ctx_exec failed to start: {e}")),
    };

    let store = match store::Store::open(db) {
        Ok(s) => s,
        Err(e) => {
            result.cleanup();
            return tool_error(format!("ctx_exec: KB open failed: {e}"));
        }
    };
    let run_id = match store.begin_run(
        command,
        &result.workdir.to_string_lossy(),
        result.sandbox,
    ) {
        Ok(id) => id,
        Err(e) => {
            result.cleanup();
            return tool_error(format!("ctx_exec: {e}"));
        }
    };
    let index = |path: &std::path::Path, stream: &str| -> Result<store::StreamStat, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        store.index_stream(run_id, stream, BufReader::new(file))
    };
    let (out_stat, err_stat) = match (
        index(&result.stdout_path(), "stdout"),
        index(&result.stderr_path(), "stderr"),
    ) {
        (Ok(o), Ok(e)) => (o, e),
        (Err(e), _) | (_, Err(e)) => {
            result.cleanup();
            return tool_error(format!("ctx_exec: index failed: {e}"));
        }
    };
    let run_label = match store.finish_run(
        run_id,
        label,
        result.exit_code,
        result.timed_out,
        result.duration_ms,
        out_stat,
        err_stat,
    ) {
        Ok(l) => l,
        Err(e) => {
            result.cleanup();
            return tool_error(format!("ctx_exec: {e}"));
        }
    };
    let status = match (result.timed_out, result.exit_code) {
        (true, _) => format!("timed out ({}s cap)", timeout_ms / 1000),
        (false, Some(code)) => format!("exit {code}"),
        (false, None) => "killed".to_string(),
    };
    let text = format!(
        "ctx_exec {run_label} — {status} in {:.2}s · sandbox: {}\n\
         stdout: {} lines ({}) · stderr: {} lines ({}) — indexed, not echoed\n\
         retrieve: ctx_search(query: \"...\", source: \"{run_label}\")",
        result.duration_ms as f64 / 1000.0,
        result.sandbox,
        out_stat.lines,
        human_bytes(out_stat.bytes),
        err_stat.lines,
        human_bytes(err_stat.bytes),
    );
    result.cleanup();
    json!({ "content": text })
}

/// `ctx_search` handler: FTS5 query → matching lines only.
fn ctx_search(db: &std::path::Path, params: &Value) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let mut queries: Vec<String> = Vec::new();
    if let Some(arr) = args.get("queries").and_then(Value::as_array) {
        queries.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
    }
    if queries.is_empty() {
        if let Some(q) = args.get("query").and_then(Value::as_str) {
            queries.push(q.to_string());
        }
    }
    if queries.is_empty() {
        return tool_error("ctx_search requires 'query' (string) or 'queries' (array)");
    }
    let source = args.get("source").and_then(Value::as_str);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(1, MAX_LIMIT as u64) as u32)
        .unwrap_or(DEFAULT_LIMIT);
    let context = args
        .get("context")
        .and_then(Value::as_u64)
        .map(|n| n.clamp(0, MAX_CONTEXT as u64) as u32)
        .unwrap_or(DEFAULT_CONTEXT);
    let stream = args.get("stream").and_then(Value::as_str);

    let store = match store::Store::open(db) {
        Ok(s) => s,
        Err(e) => return tool_error(format!("ctx_search: KB open failed: {e}")),
    };
    if store.run_count() == 0 {
        return json!({ "content": "Knowledge base is empty — run ctx_exec first; its output is what ctx_search queries." });
    }

    let mut sections: Vec<String> = Vec::new();
    let mut budget = RESPONSE_CAP;
    'outer: for query in &queries {
        let hits = match store.search(query, source, stream, limit) {
            Ok(h) => h,
            Err(e) => {
                sections.push(format!("query {query:?} — search failed: {e}"));
                continue;
            }
        };
        if hits.is_empty() {
            let known = store
                .sources(8)
                .map(|rows| {
                    rows.iter()
                        .map(|(_, label, lines)| format!("{label} ({lines} lines)"))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            sections.push(format!(
                "query {query:?} — no matches.\nindexed runs: {known}"
            ));
            continue;
        }
        let mut section = String::new();
        for hit in &hits {
            let mut lines: Vec<&str> = hit.highlighted.split('\n').collect();
            // A trailing newline leaves a phantom empty element; drop it so
            // it can't be emitted or inflate the reported range.
            if lines.last() == Some(&"") {
                lines.pop();
            }
            let mut emit = vec![false; lines.len()];
            for (i, line) in lines.iter().enumerate() {
                if line.contains('\u{2}') {
                    let lo = i.saturating_sub(context as usize);
                    let hi = (i + context as usize + 1).min(lines.len());
                    for j in lo..hi {
                        emit[j] = true;
                    }
                }
            }
            // Matched but nothing markable (e.g. highlight() NULL fallback):
            // show a head preview so the hit isn't silent.
            if !emit.iter().any(|e| *e) {
                for e in emit.iter_mut().take(3) {
                    *e = true;
                }
            }
            let mut kept: Vec<String> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if !emit[i] {
                    continue;
                }
                let clean = line.replace(['\u{2}', '\u{3}'], "");
                let clean = if clean.chars().count() > LINE_CAP {
                    format!("{}…", clean.chars().take(LINE_CAP).collect::<String>())
                } else {
                    clean
                };
                kept.push(format!("L{}: {clean}", hit.start_line + i as u64));
                if kept.len() >= 40 {
                    break;
                }
            }
            let end_line = hit.start_line + lines.len() as u64 - 1;
            section.push_str(&format!(
                "── {} {} L{}-L{} ──\n{}\n",
                hit.label,
                hit.stream,
                hit.start_line,
                end_line,
                kept.join("\n")
            ));
            budget = budget.saturating_sub(kept.join("\n").len() + 96);
            if budget < 512 {
                section.push_str("…response cap reached; narrow with source:/limit:/context:\n");
                sections.push(format!("query {query:?}:\n{section}"));
                break 'outer;
            }
        }
        let header = format!("query {query:?} — {} chunk(s) matched:", hits.len());
        sections.push(format!("{header}\n{section}"));
    }
    json!({ "content": sections.join("\n") })
}

/// One protocol call → result value. `db` is the KB path (usually
/// `store::db_path()`; injected for tests).
pub fn dispatch(method: &str, params: &Value, db: &std::path::Path) -> Value {
    match method {
        "plugin/manifest" => manifest(),
        "prompt/context" => json!({ "text": prompt_guidance() }),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            match name {
                "ctx_exec" | "" => ctx_exec(db, params),
                "ctx_search" => ctx_search(db, params),
                other => tool_error(format!("unknown tool: {other}")),
            }
        }
        _ => json!({ "error": "unsupported method" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_shape() {
        let m = manifest();
        assert_eq!(m["name"], "context-mode");
        assert_eq!(m["protocol"], "1.1");
        assert_eq!(m["sidecar_args"], json!(["sidecar"]));
        assert_eq!(m["tools"][0]["name"], "ctx_exec");
        assert_eq!(m["tools"][1]["name"], "ctx_search");
        assert_eq!(m["hooks"], json!(["prompt/context"]));
    }
}
