//! SQLite + FTS5 knowledge base for captured command output.
//!
//! Two tables: `runs` (one row per ctx_exec — command, cwd, exit code,
//! per-stream byte/line counts) and `chunks` (FTS5 over ~120-line slices of
//! each stream, so a MATCH hit maps back to run + stream + line offset).
//! Search returns the matching *lines* from each hit chunk — the context
//! window stays small even when the captured output is huge.

use rusqlite::{params, Connection};
use std::io::BufRead;
use std::path::Path;

/// Lines per indexed chunk — small enough that a hit is a tight window,
/// large enough that a 5k-line log is ~45 rows.
pub const CHUNK_LINES: usize = 120;
/// Bytes per indexed chunk (line-boundary clamped; a chunk may exceed this
/// by one line when lines are long).
pub const CHUNK_BYTES: usize = 8 * 1024;
/// Runs retained; oldest are pruned on each exec so the KB stays bounded.
pub const MAX_RUNS: i64 = 300;

pub struct Store {
    conn: Connection,
}

/// Per-stream counts collected while indexing.
#[derive(Debug, Default, Clone, Copy)]
pub struct StreamStat {
    pub lines: u64,
    pub bytes: u64,
}

/// One search hit: a chunk that matched, with its highlighted content for
/// line extraction.
#[derive(Debug)]
pub struct Hit {
    pub run_id: i64,
    pub label: String,
    pub stream: String,
    pub start_line: u64,
    /// Chunk content with matched terms wrapped in \x02..\x03.
    pub highlighted: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let conn =
            Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "busy_timeout", 5000i64);
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                label TEXT NOT NULL,
                command TEXT NOT NULL DEFAULT '',
                cwd TEXT NOT NULL DEFAULT '',
                sandbox TEXT NOT NULL DEFAULT '',
                exit_code INTEGER,
                timed_out INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                stdout_lines INTEGER NOT NULL DEFAULT 0,
                stdout_bytes INTEGER NOT NULL DEFAULT 0,
                stderr_lines INTEGER NOT NULL DEFAULT 0,
                stderr_bytes INTEGER NOT NULL DEFAULT 0,
                started_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
                content,
                run_id UNINDEXED,
                stream UNINDEXED,
                start_line UNINDEXED,
                tokenize='porter unicode61'
            );
            CREATE INDEX IF NOT EXISTS idx_runs_label ON runs(label);",
        )
        .map_err(|e| format!("schema: {e}"))?;
        // Pre-change DBs carry a chunks table with an indexed `title`
        // column that produced title-only hits with no displayable lines.
        // (table_xinfo also lists FTS5's hidden columns, so detect the old
        // shape by the `title` name rather than a column count.)
        let has_title: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_xinfo('chunks') WHERE name = 'title'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_title > 0 {
            conn.execute_batch(
                "DROP TABLE chunks;
                 CREATE VIRTUAL TABLE chunks USING fts5(
                    content,
                    run_id UNINDEXED,
                    stream UNINDEXED,
                    start_line UNINDEXED,
                    tokenize='porter unicode61'
                 );",
            )
            .map_err(|e| format!("schema migration: {e}"))?;
        }
        Ok(Store { conn })
    }

    /// Insert a run row; returns its id (the `exec:<id>` label is stamped by
    /// `finish_run` once the caller knows the final counts).
    pub fn begin_run(&self, command: &str, cwd: &str, sandbox: &str) -> Result<i64, String> {
        let command: String = command.chars().take(400).collect();
        self.conn
            .execute(
                "INSERT INTO runs (label, command, cwd, sandbox) VALUES ('', ?1, ?2, ?3)",
                params![command, cwd, sandbox],
            )
            .map_err(|e| format!("begin_run: {e}"))?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_run(
        &self,
        id: i64,
        user_label: Option<&str>,
        exit_code: Option<i32>,
        timed_out: bool,
        duration_ms: u64,
        out: StreamStat,
        err: StreamStat,
    ) -> Result<String, String> {
        let label = match user_label.map(str::trim).filter(|s| !s.is_empty()) {
            Some(extra) => format!("exec:{id}:{}", slug(extra)),
            None => format!("exec:{id}"),
        };
        self.conn
            .execute(
                "UPDATE runs SET label=?1, exit_code=?2, timed_out=?3, duration_ms=?4,
                    stdout_lines=?5, stdout_bytes=?6, stderr_lines=?7, stderr_bytes=?8
                 WHERE id=?9",
                params![
                    label,
                    exit_code,
                    timed_out as i64,
                    duration_ms as i64,
                    out.lines as i64,
                    out.bytes as i64,
                    err.lines as i64,
                    err.bytes as i64,
                    id
                ],
            )
            .map_err(|e| format!("finish_run: {e}"))?;
        self.prune()?;
        Ok(label)
    }

    /// Index one captured stream (stdout/stderr) into FTS5 chunks.
    /// Byte-exact on disk; lossy UTF-8 in the index.
    pub fn index_stream(
        &self,
        run_id: i64,
        stream: &str,
        mut reader: impl BufRead,
    ) -> Result<StreamStat, String> {
        let mut stat = StreamStat::default();
        let mut chunk = String::new();
        let mut chunk_start = 1u64;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = reader
                .read_until(b'\n', &mut buf)
                .map_err(|e| format!("read {stream}: {e}"))?;
            if n == 0 {
                break;
            }
            stat.lines += 1;
            stat.bytes += n as u64;
            chunk.push_str(&String::from_utf8_lossy(&buf));
            if stat.lines - chunk_start + 1 >= CHUNK_LINES as u64
                || chunk.len() >= CHUNK_BYTES
            {
                self.insert_chunk(run_id, stream, chunk_start, &chunk)?;
                chunk.clear();
                chunk_start = stat.lines + 1;
            }
        }
        if !chunk.is_empty() {
            self.insert_chunk(run_id, stream, chunk_start, &chunk)?;
        }
        Ok(stat)
    }

    fn insert_chunk(
        &self,
        run_id: i64,
        stream: &str,
        start_line: u64,
        content: &str,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO chunks (content, run_id, stream, start_line)
                 VALUES (?1, ?2, ?3, ?4)",
                params![content, run_id as i64, stream, start_line as i64],
            )
            .map_err(|e| format!("insert chunk: {e}"))?;
        Ok(())
    }

    /// FTS5 search. `source` scopes to `exec:<id>` (exact) or a label
    /// substring. Tries the raw query first; on FTS5 syntax error or zero
    /// hits retries with every token double-quoted (AND semantics).
    pub fn search(
        &self,
        query: &str,
        source: Option<&str>,
        stream: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Hit>, String> {
        match self.search_inner(query, source, stream, limit) {
            Ok(hits) if !hits.is_empty() => Ok(hits),
            Ok(_) => {
                let fallback = sanitized(query);
                if fallback.as_deref() == Some(query) {
                    return Ok(Vec::new());
                }
                match fallback {
                    Some(q) => self.search_inner(&q, source, stream, limit),
                    None => Ok(Vec::new()),
                }
            }
            Err(_) => match sanitized(query) {
                Some(q) => self.search_inner(&q, source, stream, limit),
                None => Err(format!("unusable query: {query:?}")),
            },
        }
    }

    fn search_inner(
        &self,
        query: &str,
        source: Option<&str>,
        stream: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Hit>, String> {
        let (src_id, src_like) = source_filter(source);
        let stream = stream
            .filter(|s| matches!(*s, "stdout" | "stderr"))
            .unwrap_or("");
        // Fixed statement shape (params ?1..?5 always bound): a missing
        // `source` leaves ?3 as "" → LIKE '%%' is a no-op; a missing/bogus
        // `stream` leaves ?5 as "" → the OR branch is a no-op.
        let sql = "SELECT chunks.run_id, runs.label, chunks.stream, chunks.start_line,
                    highlight(chunks, 0, char(2), char(3)), chunks.content
             FROM chunks JOIN runs ON runs.id = chunks.run_id
             WHERE chunks MATCH ?1
               AND (runs.id = ?2 OR runs.label LIKE '%' || ?3 || '%')
               AND (chunks.stream = ?5 OR ?5 = '')
             ORDER BY bm25(chunks)
             LIMIT ?4";
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| format!("search: {e}"))?;
        let rows = stmt
            .query_map(
                params![query, src_id, src_like, limit as i64, stream],
                |row| {
                Ok(Hit {
                    run_id: row.get(0)?,
                    label: row.get(1)?,
                    stream: row.get(2)?,
                    start_line: row.get::<_, i64>(3)? as u64,
                    highlighted: row
                        .get::<_, Option<String>>(4)?
                        .or_else(|| row.get::<_, Option<String>>(5).ok().flatten())
                        .unwrap_or_default(),
                })
            },
            )
            .map_err(|e| format!("search: {e}"))?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row.map_err(|e| format!("search row: {e}"))?);
        }
        Ok(hits)
    }

    /// `(runs, chunks, db bytes on disk)` for `stats`.
    pub fn stats(&self) -> Result<(i64, i64), String> {
        let runs: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let chunks: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok((runs, chunks))
    }

    /// Labels of recent runs, newest first — used in "no hits" guidance.
    pub fn sources(&self, limit: i64) -> Result<Vec<(i64, String, i64)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, label, stdout_lines + stderr_lines FROM runs
                 ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    pub fn run_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Delete everything. Returns the number of runs dropped.
    pub fn clear(&self) -> Result<i64, String> {
        let n = self.run_count();
        self.conn
            .execute_batch("DELETE FROM chunks; DELETE FROM runs;")
            .map_err(|e| e.to_string())?;
        Ok(n)
    }

    /// Drop runs (and their chunks) beyond MAX_RUNS.
    fn prune(&self) -> Result<(), String> {
        self.conn
            .execute_batch(&format!(
                "DELETE FROM chunks WHERE run_id NOT IN
                    (SELECT id FROM runs ORDER BY id DESC LIMIT {MAX_RUNS});
                 DELETE FROM runs WHERE id NOT IN
                    (SELECT id FROM runs ORDER BY id DESC LIMIT {MAX_RUNS});"
            ))
            .map_err(|e| format!("prune: {e}"))
    }
}

/// Label-safe slug for user-supplied run names.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(48)
        .collect()
}

/// `(id_param, like_param)` — `exec:N`/`N` scopes by run id, anything else
/// is a case-insensitive label substring. Missing source → (0, ""), which
/// makes the SQL predicate a no-op.
fn source_filter(source: Option<&str>) -> (i64, String) {
    let Some(src) = source.map(str::trim).filter(|s| !s.is_empty()) else {
        return (0, String::new());
    };
    let id = src
        .strip_prefix("exec:")
        .unwrap_or(src)
        .parse::<i64>()
        .unwrap_or(-1);
    (id, src.to_string())
}

/// Quote every whitespace-separated token so plain words can't hit FTS5
/// syntax (`error:`, `foo-bar`, `a OR b` all become literal term ANDs).
/// Returns None when nothing tokenizable remains.
fn sanitized(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter_map(|t| {
            let t = t.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
            if t.is_empty() {
                None
            } else {
                Some(format!("\"{}\"", t.replace('"', "")))
            }
        })
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// Resolve the KB path: `$GRAY_HOME/context-mode/kb.sqlite`, else
/// `~/.gray/context-mode/kb.sqlite`.
pub fn db_path() -> std::path::PathBuf {
    let home = std::env::var("GRAY_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| user_home().map(|h| h.join(".gray")))
        .unwrap_or_else(|| std::env::temp_dir().join("gray"));
    home.join("context-mode").join("kb.sqlite")
}

fn user_home() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let path = std::env::var_os("HOMEPATH")?;
                Some(std::path::PathBuf::from(format!(
                    "{}{}",
                    drive.to_string_lossy(),
                    path.to_string_lossy()
                )))
            })
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kb.sqlite");
        (dir, path)
    }

    fn indexed(store: &Store, run_id: i64, stream: &str, text: &str) -> StreamStat {
        store
            .index_stream(run_id, stream, Cursor::new(text.as_bytes()))
            .unwrap()
    }

    #[test]
    fn index_then_search_finds_lines() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let id = store.begin_run("seq 3", "/tmp", "cwd").unwrap();
        let stat = indexed(&store, id, "stdout", "alpha\nbeta error\ngamma\n");
        assert_eq!((stat.lines, stat.bytes), (3, 23));
        store
            .finish_run(id, None, Some(0), false, 5, stat, StreamStat::default())
            .unwrap();
        let hits = store.search("error", None, None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label, "exec:1");
        assert_eq!(hits[0].stream, "stdout");
        assert_eq!(hits[0].start_line, 1);
        assert!(hits[0].highlighted.contains("beta"));
        // The match term is wrapped for line extraction.
        assert!(hits[0].highlighted.contains('\u{2}'));
    }

    #[test]
    fn source_scopes_by_id_and_label() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let a = store.begin_run("a", "/x", "cwd").unwrap();
        indexed(&store, a, "stdout", "needle one\n");
        store
            .finish_run(a, Some("first"), Some(0), false, 1, StreamStat::default(), StreamStat::default())
            .unwrap();
        let b = store.begin_run("b", "/x", "cwd").unwrap();
        indexed(&store, b, "stdout", "needle two\n");
        store
            .finish_run(b, Some("second"), Some(0), false, 1, StreamStat::default(), StreamStat::default())
            .unwrap();

        assert_eq!(store.search("needle", Some("exec:1"), None, 10).unwrap().len(), 1);
        assert_eq!(store.search("needle", Some("exec:2"), None, 10).unwrap().len(), 1);
        assert_eq!(store.search("needle", Some("second"), None, 10).unwrap().len(), 1);
        assert_eq!(store.search("needle", None, None, 10).unwrap().len(), 2);
        let hits = store.search("needle", Some("exec:1"), None, 10).unwrap();
        assert_eq!(hits[0].label, "exec:1:first");
    }

    #[test]
    fn plain_words_survive_fts_syntax_chars() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let id = store.begin_run("c", "/x", "cwd").unwrap();
        indexed(&store, id, "stderr", "error[E0308]: mismatched types\n");
        store
            .finish_run(id, None, Some(1), false, 1, StreamStat::default(), StreamStat::default())
            .unwrap();
        // `error[E0308]` would explode raw FTS5 (bad column); fallback finds it.
        let hits = store.search("mismatched types", None, None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(store.search("OR (broken", None, None, 10).is_ok());
    }

    #[test]
    fn chunks_split_on_line_boundaries() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let id = store.begin_run("d", "/x", "cwd").unwrap();
        let text: String = (1..=300).map(|i| format!("line {i}\n")).collect();
        let stat = indexed(&store, id, "stdout", &text);
        assert_eq!(stat.lines, 300);
        // 300 lines / 120-line chunks → 3 chunks; last starts at line 241.
        let hits = store.search("line 241", None, None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].start_line, 241);
    }

    #[test]
    fn clear_wipes_runs() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let id = store.begin_run("e", "/x", "cwd").unwrap();
        indexed(&store, id, "stdout", "x\n");
        store
            .finish_run(id, None, Some(0), false, 1, StreamStat::default(), StreamStat::default())
            .unwrap();
        assert_eq!(store.clear().unwrap(), 1);
        assert_eq!(store.run_count(), 0);
    }
}
