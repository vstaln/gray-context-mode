//! `gray-context-mode` — sidecar + small CLI for the context-mode plugin.
//!
//! `ctx_exec` runs a shell command in a sandboxed scratch dir and indexes
//! all stdout/stderr into a SQLite FTS5 store (returning only a summary);
//! `ctx_search` pulls back just the matching lines. Huge outputs stay out
//! of context; the agent queries for the slice it needs.
//!
//! Wire: NDJSON on stdio — see ~/grayplugins/PORTING.md. Bare argv runs
//! the sidecar loop; `manifest` prints the install manifest; `exec`,
//! `search`, `stats`, `clear` are the CLI verbs.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

fn db() -> PathBuf {
    gray_context_mode::store::db_path()
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
fn handle(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        // Notifications need no reply; shutdown exits the loop.
        return (None, method == "plugin/shutdown");
    };
    let result = gray_context_mode::dispatch(method, &params, &db());
    if method == "plugin/shutdown" {
        return (Some(json!({ "id": id, "result": {} })), true);
    }
    if let Some(err) = result.get("error") {
        let error = json!({ "code": -32601, "message": err });
        return (Some(json!({ "id": id, "error": error })), false);
    }
    (Some(json!({ "id": id, "result": result })), false)
}

fn sidecar() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle(&req);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

/// CLI: run a command through the same exec+index path the wire uses.
fn cli_exec(argv: &[String]) -> i32 {
    let mut words: Vec<String> = Vec::new();
    let mut args = json!({});
    let mut it = argv.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--cwd" | "--label" | "--timeout-ms" => {
                let Some(v) = it.next() else {
                    eprintln!("gray-context-mode: {a} needs a value");
                    return 2;
                };
                let key = match a.as_str() {
                    "--cwd" => "cwd",
                    "--label" => "label",
                    _ => "timeout_ms",
                };
                args[key] = if key == "timeout_ms" {
                    json!(v.parse::<u64>().unwrap_or(0))
                } else {
                    json!(v)
                };
            }
            _ => words.push(a.clone()),
        }
    }
    let command = words.join(" ");
    if command.trim().is_empty() {
        eprintln!("usage: gray-context-mode exec <command...> [--cwd DIR] [--label NAME] [--timeout-ms N]");
        return 2;
    }
    args["command"] = json!(command);
    let req = json!({"name": "ctx_exec", "args": args});
    let out = gray_context_mode::dispatch("tool/call", &req, &db());
    if let Some(text) = out.get("content").and_then(Value::as_str) {
        println!("{text}");
        if out.get("is_error").and_then(Value::as_bool) == Some(true) {
            return 1;
        }
        0
    } else {
        eprintln!("{out}");
        1
    }
}

fn cli_search(argv: &[String]) -> i32 {
    let Some(query) = argv.first() else {
        eprintln!("usage: gray-context-mode search <query> [source] [limit]");
        return 2;
    };
    let mut args = json!({ "query": query });
    if let Some(src) = argv.get(1) {
        args["source"] = json!(src);
    }
    if let Some(n) = argv.get(2).and_then(|s| s.parse::<u64>().ok()) {
        args["limit"] = json!(n);
    }
    let out = gray_context_mode::dispatch("tool/call", &json!({"name":"ctx_search","args":args}), &db());
    if let Some(text) = out.get("content").and_then(Value::as_str) {
        println!("{text}");
        if out.get("is_error").and_then(Value::as_bool) == Some(true) {
            return 1;
        }
        0
    } else {
        eprintln!("{out}");
        1
    }
}

fn cli_stats() -> i32 {
    let path = db();
    match gray_context_mode::store::Store::open(&path).and_then(|s| s.stats()) {
        Ok((runs, chunks)) => {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("{} — {runs} runs, {chunks} chunks, {} KB", path.display(), size / 1024);
            0
        }
        Err(e) => {
            eprintln!("gray-context-mode: stats: {e}");
            1
        }
    }
}

fn cli_clear() -> i32 {
    match gray_context_mode::store::Store::open(&db()).and_then(|s| s.clear()) {
        Ok(n) => {
            println!("cleared {n} run(s) from the knowledge base");
            0
        }
        Err(e) => {
            eprintln!("gray-context-mode: clear: {e}");
            1
        }
    }
}

const USAGE: &str = "gray-context-mode — sandboxed exec + FTS5 KB plugin for gray

usage:
  gray-context-mode                    run the sidecar wire loop (stdin/stdout NDJSON)
  gray-context-mode sidecar            same, explicitly
  gray-context-mode manifest           print the install manifest (JSON)
  gray-context-mode exec <command...>  run a command sandboxed; index output, print summary
  gray-context-mode search <query> [source] [limit]
                                       print matching lines from the KB
  gray-context-mode stats              show KB path, run and chunk counts
  gray-context-mode clear              delete all captured runs
";

fn cli(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        None | Some("sidecar") => {
            if let Err(e) = sidecar() {
                eprintln!("gray-context-mode: sidecar: {e}");
                return 1;
            }
            0
        }
        Some("manifest") => {
            println!("{}", gray_context_mode::manifest());
            0
        }
        Some("exec") => cli_exec(&argv[1..]),
        Some("search") => cli_search(&argv[1..]),
        Some("stats") => cli_stats(),
        Some("clear") => cli_clear(),
        Some("help") | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("gray-context-mode: unknown subcommand {other:?}\n\n{USAGE}");
            2
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cli(&argv));
}
