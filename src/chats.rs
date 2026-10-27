// ============================================================================
// Conversations — every question and answer, saved on this machine.
//
// One SQLite file, ~/.searchwala/searchwala.db (SEARCHWALA_DATA_DIR overrides
// the directory). A conversation is an ordered list of turns; a turn keeps the
// question, the query actually sent to the engines, the final answer, and the
// response the UI rendered (sources, quick answer, citations), so reopening a
// conversation shows it exactly as it was, without searching again.
//
// A turn is written when the question is asked (status "running") and
// completed when the pipeline finishes, so closing the tab mid-search never
// loses the question, and the answer still lands.
//
// Replaces history.jsonl, which kept only the answer text, could only be
// cleared as a whole, and was off unless the user opted in.
// ============================================================================

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub turns: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    pub id: i64,
    pub query: String,
    /// The standalone query sent to the engines, when memory rewrote a
    /// follow-up ("and its battery?" → "iPhone 17 battery life").
    pub search_query: Option<String>,
    pub mode: String,
    /// running | done | stopped | failed | interrupted
    pub status: String,
    pub answer: Option<String>,
    /// The search response as the UI rendered it, without passage text.
    pub response: Option<serde_json::Value>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub turns: Vec<Turn>,
}

/// An earlier question and its answer, as memory for the next one.
#[derive(Debug, Clone, PartialEq)]
pub struct PriorTurn {
    pub query: String,
    pub answer: String,
}

#[derive(Clone)]
pub struct ChatStore {
    conn: Arc<Mutex<Connection>>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// First line of the question, cut at a word boundary.
pub fn title_from(query: &str) -> String {
    let q = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if q.chars().count() <= 80 {
        return q;
    }
    let cut: String = q.chars().take(80).collect();
    match cut.rfind(' ') {
        Some(i) if i > 40 => format!("{}…", &cut[..i]),
        _ => format!("{cut}…"),
    }
}

/// The response minus passage text: `results` is only needed for citation
/// targets, so each keeps its URL and title. Passages are most of the bytes.
pub fn slim_response(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(results) = v.get_mut("results").and_then(|r| r.as_array_mut()) {
        for r in results.iter_mut() {
            let url = r.get("url").cloned().unwrap_or_default();
            let title = r.get("title").cloned().unwrap_or_default();
            *r = serde_json::json!({ "url": url, "title": title });
        }
    }
    v
}

impl ChatStore {
    /// ~/.searchwala, or SEARCHWALA_DATA_DIR.
    pub fn data_dir() -> PathBuf {
        if let Ok(d) = std::env::var("SEARCHWALA_DATA_DIR") {
            if !d.trim().is_empty() {
                return PathBuf::from(d);
            }
        }
        let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".searchwala")
    }

    pub fn open_default() -> Result<Self, String> {
        let dir = Self::data_dir();
        let store = Self::open(&dir.join("searchwala.db"))?;
        match store.import_legacy(&dir) {
            Ok(0) => {}
            Ok(n) => tracing::info!("chats: imported {n} entries from the old history files"),
            Err(e) => tracing::warn!("chats: old history not imported, files left in place: {e}"),
        }
        Ok(store)
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, String> {
        Self::init(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             -- Deleted conversations are overwritten on disk, not just unlinked.
             PRAGMA secure_delete = ON;
             CREATE TABLE IF NOT EXISTS conversations (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS turns (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                 query TEXT NOT NULL,
                 search_query TEXT,
                 mode TEXT NOT NULL,
                 status TEXT NOT NULL,
                 answer TEXT,
                 response TEXT,
                 created_at INTEGER NOT NULL,
                 finished_at INTEGER
             );
             CREATE INDEX IF NOT EXISTS turns_by_conversation ON turns(conversation_id, id);
             CREATE INDEX IF NOT EXISTS conversations_by_update ON conversations(updated_at DESC);
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             -- A turn still running when the server stopped will never finish.
             UPDATE turns SET status = 'interrupted' WHERE status = 'running';",
        )
        .map_err(|e| format!("init: {e}"))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Runs `f` on the connection off the async runtime.
    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut c)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
    }

    // ── Settings ────────────────────────────────────────────────────────────

    /// Saving is on unless the user paused it.
    pub async fn saving_enabled(&self) -> bool {
        self.with(|c| {
            c.query_row("SELECT value FROM settings WHERE key = 'saving'", [], |r| r.get::<_, String>(0)).optional()
        })
        .await
        .ok()
        .flatten()
        .is_none_or(|v| v != "off")
    }

    pub async fn set_saving(&self, on: bool) -> Result<(), String> {
        let v = if on { "on" } else { "off" };
        self.with(move |c| {
            c.execute(
                "INSERT INTO settings(key, value) VALUES ('saving', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [v],
            )
            .map(|_| ())
        })
        .await
    }

    // ── Turns ───────────────────────────────────────────────────────────────

    /// Records a new question. Continues `conversation_id` when it exists,
    /// otherwise starts a conversation titled after the question.
    pub async fn begin_turn(&self, conversation_id: Option<String>, query: &str, mode: &str) -> Result<(String, i64), String> {
        let (query, mode) = (query.to_string(), mode.to_string());
        self.with(move |c| {
            let tx = c.transaction()?;
            let now = now_ms();
            let existing = match conversation_id {
                Some(id) => tx.query_row("SELECT id FROM conversations WHERE id = ?1", [&id], |r| r.get::<_, String>(0)).optional()?,
                None => None,
            };
            let conv = match existing {
                Some(id) => {
                    tx.execute("UPDATE conversations SET updated_at = ?2 WHERE id = ?1", params![id, now])?;
                    id
                }
                None => {
                    let id = new_id();
                    tx.execute(
                        "INSERT INTO conversations(id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                        params![id, title_from(&query), now],
                    )?;
                    id
                }
            };
            tx.execute(
                "INSERT INTO turns(conversation_id, query, mode, status, created_at) VALUES (?1, ?2, ?3, 'running', ?4)",
                params![conv, query, mode, now],
            )?;
            let turn = tx.last_insert_rowid();
            tx.commit()?;
            Ok((conv, turn))
        })
        .await
    }

    pub async fn finish_turn(
        &self,
        turn_id: i64,
        status: &str,
        search_query: Option<String>,
        answer: Option<String>,
        response: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let status = status.to_string();
        let response = response.map(|v| slim_response(v).to_string());
        self.with(move |c| {
            let now = now_ms();
            // A turn the user stopped stays stopped even if the pipeline
            // finishes a moment later.
            c.execute(
                "UPDATE turns SET status = ?2, search_query = ?3, answer = ?4, response = ?5, finished_at = ?6
                 WHERE id = ?1 AND status = 'running'",
                params![turn_id, status, search_query, answer, response, now],
            )?;
            c.execute(
                "UPDATE conversations SET updated_at = ?2 WHERE id = (SELECT conversation_id FROM turns WHERE id = ?1)",
                params![turn_id, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Marks a running turn stopped. False when it was not running.
    pub async fn stop_turn(&self, turn_id: i64) -> Result<bool, String> {
        self.with(move |c| {
            c.execute(
                "UPDATE turns SET status = 'stopped', finished_at = ?2 WHERE id = ?1 AND status = 'running'",
                params![turn_id, now_ms()],
            )
            .map(|n| n > 0)
        })
        .await
    }

    /// The last `limit` answered turns of a conversation, oldest first,
    /// excluding `before_turn` and anything after it.
    pub async fn prior_turns(&self, conversation_id: &str, before_turn: i64, limit: usize) -> Result<Vec<PriorTurn>, String> {
        let id = conversation_id.to_string();
        let limit = limit.min(10_000) as i64;
        self.with(move |c| {
            let mut stmt = c.prepare(
                "SELECT query, answer FROM turns
                 WHERE conversation_id = ?1 AND id < ?2 AND status = 'done' AND answer IS NOT NULL AND answer != ''
                 ORDER BY id DESC LIMIT ?3",
            )?;
            let mut v = stmt
                .query_map(params![id, before_turn, limit], |r| Ok(PriorTurn { query: r.get(0)?, answer: r.get(1)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v.reverse();
            Ok(v)
        })
        .await
    }

    // ── Conversations ───────────────────────────────────────────────────────

    /// Most recently active first. `filter` matches titles and questions.
    pub async fn list(&self, filter: Option<String>, limit: usize) -> Result<Vec<ConversationSummary>, String> {
        let limit = limit.clamp(1, 10_000) as i64;
        self.with(move |c| {
            let like = filter.filter(|f| !f.trim().is_empty()).map(|f| format!("%{}%", f.trim().replace('%', "\\%").replace('_', "\\_")));
            let mut stmt = c.prepare(
                "SELECT c.id, c.title, c.created_at, c.updated_at, (SELECT COUNT(*) FROM turns t WHERE t.conversation_id = c.id)
                 FROM conversations c
                 WHERE ?1 IS NULL OR c.title LIKE ?1 ESCAPE '\\'
                    OR EXISTS (SELECT 1 FROM turns t WHERE t.conversation_id = c.id AND t.query LIKE ?1 ESCAPE '\\')
                 ORDER BY c.updated_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![like, limit], |r| {
                Ok(ConversationSummary { id: r.get(0)?, title: r.get(1)?, created_at: r.get(2)?, updated_at: r.get(3)?, turns: r.get(4)? })
            })?;
            rows.collect()
        })
        .await
    }

    pub async fn get(&self, id: &str) -> Result<Option<Conversation>, String> {
        let id = id.to_string();
        self.with(move |c| {
            let head = c
                .query_row("SELECT id, title, created_at, updated_at FROM conversations WHERE id = ?1", [&id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
                })
                .optional()?;
            let Some((id, title, created_at, updated_at)) = head else { return Ok(None) };
            let mut stmt = c.prepare(
                "SELECT id, query, search_query, mode, status, answer, response, created_at, finished_at
                 FROM turns WHERE conversation_id = ?1 ORDER BY id",
            )?;
            let turns = stmt
                .query_map([&id], |r| {
                    let response: Option<String> = r.get(6)?;
                    Ok(Turn {
                        id: r.get(0)?,
                        query: r.get(1)?,
                        search_query: r.get(2)?,
                        mode: r.get(3)?,
                        status: r.get(4)?,
                        answer: r.get(5)?,
                        response: response.and_then(|s| serde_json::from_str(&s).ok()),
                        created_at: r.get(7)?,
                        finished_at: r.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Some(Conversation { id, title, created_at, updated_at, turns }))
        })
        .await
    }

    pub async fn rename(&self, id: &str, title: &str) -> Result<bool, String> {
        let (id, title) = (id.to_string(), title_from(title));
        if title.is_empty() {
            return Ok(false);
        }
        self.with(move |c| c.execute("UPDATE conversations SET title = ?2 WHERE id = ?1", params![id, title]).map(|n| n > 0))
            .await
    }

    pub async fn delete(&self, id: &str) -> Result<bool, String> {
        let id = id.to_string();
        self.with(move |c| c.execute("DELETE FROM conversations WHERE id = ?1", [id]).map(|n| n > 0)).await
    }

    /// Deletes every conversation and compacts the file, so nothing remains
    /// in free pages.
    pub async fn delete_all(&self) -> Result<usize, String> {
        self.with(|c| {
            let n = c.execute("DELETE FROM conversations", [])?;
            c.execute_batch("VACUUM")?;
            Ok(n)
        })
        .await
    }

    // ── Migration ───────────────────────────────────────────────────────────

    /// Imports history.jsonl and the older history.json (one conversation per
    /// entry), then deletes both files. Nothing is deleted unless the import
    /// committed. Returns the number of entries imported.
    pub fn import_legacy(&self, dir: &Path) -> Result<usize, String> {
        let files = [dir.join("history.json"), dir.join("history.jsonl")];
        let mut entries: Vec<serde_json::Value> = Vec::new();
        for f in files.iter().filter(|f| f.exists()) {
            let text = std::fs::read_to_string(f).map_err(|e| format!("read {}: {e}", f.display()))?;
            if text.trim_start().starts_with('[') {
                let v: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", f.display()))?;
                entries.extend(v);
            } else {
                entries.extend(text.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()));
            }
        }
        if entries.is_empty() {
            for f in files.iter().filter(|f| f.exists()) {
                let _ = std::fs::remove_file(f);
            }
            return Ok(0);
        }
        // The two files overlap when history.json was migrated to .jsonl.
        let mut seen = std::collections::HashSet::new();
        entries.retain(|e| seen.insert(e.get("id").map(|v| v.to_string()).unwrap_or_else(|| e.to_string())));

        let mut c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let mut imported = 0;
        for e in &entries {
            let Some(query) = e.get("query").and_then(|q| q.as_str()).filter(|q| !q.trim().is_empty()) else { continue };
            let answer = e.get("answer").and_then(|a| a.as_str()).map(str::to_string);
            let mode = e.get("focus_mode").and_then(|m| m.as_str()).unwrap_or("auto").split(':').next().unwrap_or("auto").to_string();
            let ts = e
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|t| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S UTC").ok())
                .map(|t| t.and_utc().timestamp_millis())
                .unwrap_or_else(now_ms);
            let response = serde_json::json!({
                "query": query,
                "mode": mode,
                "tier": mode,
                "domain": "general",
                "llm_answer": answer,
                "search_results": [],
                "results": [],
                "imported": true,
            });
            let id = new_id();
            tx.execute(
                "INSERT INTO conversations(id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                params![id, title_from(query), ts],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO turns(conversation_id, query, mode, status, answer, response, created_at, finished_at)
                 VALUES (?1, ?2, ?3, 'done', ?4, ?5, ?6, ?6)",
                params![id, query, mode, answer, response.to_string(), ts],
            )
            .map_err(|e| e.to_string())?;
            imported += 1;
        }
        tx.commit().map_err(|e| e.to_string())?;
        for f in files.iter().filter(|f| f.exists()) {
            if let Err(e) = std::fs::remove_file(f) {
                tracing::warn!("chats: imported but could not delete {}: {e}", f.display());
            }
        }
        Ok(imported)
    }
}

// ── Memory ──────────────────────────────────────────────────────────────────

/// Characters of one earlier answer given to the model. Enough for what was
/// said; the sources behind it are not resent.
const ANSWER_CHARS: usize = 900;

/// Earlier turns as a prompt block that fits `max_chars`, newest kept first,
/// and how many turns it holds. Citation markers are dropped: they point at
/// sources this turn doesn't have.
pub fn memory_block(turns: &[PriorTurn], max_chars: usize) -> (String, usize) {
    static CITE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\s?\[\d+(?:\s*,\s*\d+)*\]").unwrap());
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0;
    for t in turns.iter().rev() {
        let answer = CITE.replace_all(&t.answer, "");
        let answer = crate::util::truncate_chars(answer.trim(), ANSWER_CHARS);
        let part = format!("User: {}\nAssistant: {}{}", t.query.trim(), answer, if answer.len() < t.answer.trim().len() { " …" } else { "" });
        let n = part.chars().count() + 2;
        if used + n > max_chars {
            break;
        }
        used += n;
        parts.push(part);
    }
    parts.reverse();
    let n = parts.len();
    (parts.join("\n\n"), n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn a_conversation_keeps_its_turns_in_order() {
        rt().block_on(async {
            let s = ChatStore::open_in_memory().unwrap();
            let (c, t1) = s.begin_turn(None, "who invented the telephone", "auto").await.unwrap();
            s.finish_turn(t1, "done", None, Some("Bell [1].".into()), Some(serde_json::json!({"results":[{"url":"u","title":"t","extracted_text":"long"}]}))).await.unwrap();
            let (c2, t2) = s.begin_turn(Some(c.clone()), "when?", "auto").await.unwrap();
            assert_eq!(c, c2);
            let conv = s.get(&c).await.unwrap().unwrap();
            assert_eq!(conv.title, "who invented the telephone");
            assert_eq!(conv.turns.len(), 2);
            assert_eq!(conv.turns[0].status, "done");
            assert_eq!(conv.turns[1].status, "running");
            // Passage text is not stored.
            assert!(conv.turns[0].response.as_ref().unwrap()["results"][0].get("extracted_text").is_none());
            let prior = s.prior_turns(&c, t2, 10).await.unwrap();
            assert_eq!(prior, vec![PriorTurn { query: "who invented the telephone".into(), answer: "Bell [1].".into() }]);
        });
    }

    #[test]
    fn unknown_conversation_ids_start_a_new_one() {
        rt().block_on(async {
            let s = ChatStore::open_in_memory().unwrap();
            let (c, _) = s.begin_turn(Some("deleted".into()), "q", "fast").await.unwrap();
            assert_ne!(c, "deleted");
            assert_eq!(s.list(None, 50).await.unwrap().len(), 1);
        });
    }

    #[test]
    fn prior_turns_are_the_latest_answered_ones() {
        rt().block_on(async {
            let s = ChatStore::open_in_memory().unwrap();
            let (c, _) = s.begin_turn(None, "q0", "auto").await.unwrap();
            let mut last = 0;
            for i in 0..5 {
                let (_, t) = s.begin_turn(Some(c.clone()), &format!("q{}", i + 1), "auto").await.unwrap();
                // q3 failed: it has no answer to remember.
                let status = if i == 2 { "failed" } else { "done" };
                s.finish_turn(t, status, None, Some(format!("a{}", i + 1)), None).await.unwrap();
                last = t;
            }
            let (_, now) = s.begin_turn(Some(c.clone()), "next", "auto").await.unwrap();
            let p = s.prior_turns(&c, now, 3).await.unwrap();
            let qs: Vec<_> = p.iter().map(|t| t.query.as_str()).collect();
            assert_eq!(qs, ["q2", "q4", "q5"]);
            assert!(s.prior_turns(&c, last, 10).await.unwrap().iter().all(|t| t.query != "q5"));
        });
    }

    #[test]
    fn a_stopped_turn_stays_stopped() {
        rt().block_on(async {
            let s = ChatStore::open_in_memory().unwrap();
            let (c, t) = s.begin_turn(None, "q", "auto").await.unwrap();
            assert!(s.stop_turn(t).await.unwrap());
            s.finish_turn(t, "done", None, Some("late".into()), None).await.unwrap();
            let conv = s.get(&c).await.unwrap().unwrap();
            assert_eq!(conv.turns[0].status, "stopped");
            assert_eq!(conv.turns[0].answer, None);
        });
    }

    #[test]
    fn delete_and_saving_switch() {
        rt().block_on(async {
            let s = ChatStore::open_in_memory().unwrap();
            assert!(s.saving_enabled().await);
            s.set_saving(false).await.unwrap();
            assert!(!s.saving_enabled().await);
            let (a, _) = s.begin_turn(None, "alpha question", "auto").await.unwrap();
            s.begin_turn(None, "beta question", "auto").await.unwrap();
            assert_eq!(s.list(Some("alpha".into()), 50).await.unwrap().len(), 1);
            assert!(s.delete(&a).await.unwrap());
            assert!(s.get(&a).await.unwrap().is_none());
            assert_eq!(s.delete_all().await.unwrap(), 1);
            assert!(s.list(None, 50).await.unwrap().is_empty());
        });
    }

    #[test]
    fn legacy_history_is_imported_then_removed() {
        let dir = std::env::temp_dir().join(format!("sw_import_{}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("history.json"),
            r#"[{"id":"h1","query":"old one","focus_mode":"lite","answer":"A1","timestamp":"2026-04-18 20:22:42 UTC"}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("history.jsonl"),
            "{\"id\":\"h1\",\"query\":\"old one\",\"answer\":\"A1\"}\n{\"id\":\"h2\",\"query\":\"new one\",\"focus_mode\":\"auto:light\",\"answer\":null}\nnot json\n",
        )
        .unwrap();
        let s = ChatStore::open_in_memory().unwrap();
        assert_eq!(s.import_legacy(&dir).unwrap(), 2);
        assert!(!dir.join("history.json").exists() && !dir.join("history.jsonl").exists());
        rt().block_on(async {
            let all = s.list(None, 50).await.unwrap();
            assert_eq!(all.len(), 2);
            let old = all.iter().find(|c| c.title == "old one").unwrap();
            let conv = s.get(&old.id).await.unwrap().unwrap();
            assert_eq!(conv.turns[0].answer.as_deref(), Some("A1"));
            assert_eq!(conv.turns[0].mode, "lite");
            assert_eq!(conv.created_at, chrono::DateTime::parse_from_rfc3339("2026-04-18T20:22:42Z").unwrap().timestamp_millis());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_keeps_the_newest_turns_that_fit() {
        let turns: Vec<PriorTurn> = (1..=4).map(|i| PriorTurn { query: format!("q{i}"), answer: format!("answer {i} [1][2, 3]") }).collect();
        let (all, n) = memory_block(&turns, 10_000);
        assert_eq!(n, 4);
        assert!(all.starts_with("User: q1") && all.contains("User: q4"), "{all}");
        assert!(!all.contains('['), "citations stripped: {all}");
        let (tight, n) = memory_block(&turns, 60);
        assert_eq!(n, 1);
        assert!(tight.contains("q4") && !tight.contains("q1"), "{tight}");
        assert_eq!(memory_block(&turns, 5), (String::new(), 0));
    }

    #[test]
    fn titles_are_cut_at_a_word() {
        assert_eq!(title_from("  short   question "), "short question");
        let t = title_from(&"word ".repeat(40));
        assert!(t.ends_with('…') && t.chars().count() <= 81, "{t}");
    }
}
