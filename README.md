# gray-context-mode

Sandboxed exec + FTS5 knowledge base for [gray](https://github.com/gray). Port of the
pi/npm `context-mode` extension's core idea: commands whose output would flood the
context window run inside a sandbox; their stdout/stderr goes into a SQLite FTS5
store instead of the conversation. The agent gets back a summary and pulls just the
lines it needs with a search call.

## Tools

| tool | what it does |
|---|---|
| `ctx_exec` | Runs `command` via `bash -c` in a sandbox (bwrap when available — read-only host root, tmpfs `/tmp`, only the work dir bound read-write; plain cwd-restricted child otherwise). stdout/stderr are captured to files and indexed into FTS5 in ~120-line chunks. **No command output is echoed.** Returns: exit code / timeout, per-stream line+byte counts, and a run label `exec:N` (or `exec:N:<label>`). Args: `command` (required), `cwd` (defaults to the session cwd, bound read-write, else a fresh scratch dir), `timeout_ms` (default 120s, max 600s, kills the whole process group), `label`. |
| `ctx_search` | FTS5 query over everything `ctx_exec` captured. Returns only the matching lines — run label, stream, and line numbers — bounded (default 8 chunks/query, max 50; `context` 0–5 lines around each match). Args: `query` or `queries[]`, `source` (`"exec:N"` or a label substring), `limit`, `context`, `stream` (`stdout`/`stderr`). Plain words work; FTS5 operators (`AND`, `OR`, `NOT`, `"phrase"`, `prefix*`) too, with an automatic quote-everything fallback when a query isn't valid syntax. |

## Install

```sh
cargo build --release
gray plugin install ~/grayplugins/gray-context-mode/target/release/gray-context-mode
```

Manifest claims: `tools=[ctx_exec, ctx_search]`, `hooks=[prompt/context]`,
`sidecar_args=["sidecar"]`, protocol `1.1`, no commands, no capabilities.

## Setup

None — no credentials, no config. State lives at
`$GRAY_HOME/context-mode/kb.sqlite` (fallback `~/.gray/context-mode/kb.sqlite`).
The scratch work/capture dirs are under the OS temp dir and deleted after each run.

## CLI

```
gray-context-mode [sidecar]       # NDJSON wire loop (default)
gray-context-mode manifest        # install manifest JSON
gray-context-mode exec <cmd> [--cwd DIR] [--label NAME] [--timeout-ms N]
gray-context-mode search <query> [source] [limit]
gray-context-mode stats           # KB path, run and chunk counts
gray-context-mode clear           # delete all captured runs
```

## Limits

- Output is **never** returned by `ctx_exec` — always `ctx_search` for the slice.
- Sandbox: real isolation needs `bwrap` (probed once; falls back to cwd mode on
  hosts without userns). Network is not restricted.
- Under bwrap, `/tmp` is a fresh tmpfs — host `/tmp` files are invisible inside.
  Put inputs in the bound `cwd`.
- The KB keeps the last 300 runs; older runs and their chunks are pruned.
- `ctx_search` response is capped (~16 KB, 40 lines/hit) — narrow with
  `source`/`stream`/`limit` instead of widening.
- Env scrub: runtime-injection vars (`BASH_ENV`, `NODE_OPTIONS`, `RUSTFLAGS`,
  `GIT_*`, `LD_PRELOAD`, …) are stripped from the child.
