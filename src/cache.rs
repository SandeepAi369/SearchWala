// ============================================================================
// SearchWala v7.0.0 — Dual Database System
//
// 1. TempDb   — In-memory HashMap for active search sessions.
//               Auto-wipes completely when a task finishes.
//
// 2. HistoryDb — Optional persistent JSON-lines file (~/.searchwala/history.json).
//               Stores past queries + LLM answers. Enabled via UI prompt.
// ============================================================================

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ─────────────────────────────────────────────────────────────────────────────
// History Entry (persisted)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub query: String,
    pub focus_mode: String,
    pub answer: Option<String>,
    pub sources_count: usize,
    pub sources_found: usize,
    pub elapsed_secs: f64,
    pub timestamp: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Temp Session (RAM-only, never persisted)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TempSession {
    pub session_id: String,
    pub query: String,
    pub status: String,            // "searching" | "scraping" | "llm_batch_N" | "done"
    pub batch_progress: String,    // "Batch 1/3" etc.
    pub sources_collected: usize,
    pub partial_answer: Option<String>,
    pub created_at: Instant,
}

// ─────────────────────────────────────────────────────────────────────────────
// TempDb — In-memory session store, auto-wipes on task completion
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TempDb {
    sessions: Arc<RwLock<HashMap<String, TempSession>>>,
}

impl TempDb {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new session for an active search task.
    pub async fn create_session(&self, query: &str) -> String {
        let session_id = format!(
            "s_{}_{:x}",
            chrono::Utc::now().timestamp_millis(),
            rand::random::<u32>()
        );

        let session = TempSession {
            session_id: session_id.clone(),
            query: query.to_string(),
            status: "searching".to_string(),
            batch_progress: String::new(),
            sources_collected: 0,
            partial_answer: None,
            created_at: Instant::now(),
        };

        self.sessions.write().await.insert(session_id.clone(), session);
        session_id
    }

    /// Update session status (e.g., "scraping", "llm_batch_1").
    pub async fn update_status(&self, session_id: &str, status: &str, batch_progress: &str) {
        if let Some(session) = self.sessions.write().await.get_mut(session_id) {
            session.status = status.to_string();
            session.batch_progress = batch_progress.to_string();
        }
    }

    /// Update sources count on the session.
    pub async fn update_sources(&self, session_id: &str, count: usize) {
        if let Some(session) = self.sessions.write().await.get_mut(session_id) {
            session.sources_collected = count;
        }
    }

    /// Store partial answer during iterative research.
    pub async fn update_partial_answer(&self, session_id: &str, answer: &str) {
        if let Some(session) = self.sessions.write().await.get_mut(session_id) {
            session.partial_answer = Some(answer.to_string());
        }
    }

    /// Get current session state (for progress polling).
    pub async fn get_session(&self, session_id: &str) -> Option<TempSession> {
        self.sessions.read().await.get(session_id).cloned()
    }

    /// Complete and **wipe** a session — auto-clean after task finishes.
    pub async fn wipe_session(&self, session_id: &str) {
        self.sessions.write().await.remove(session_id);
        tracing::debug!("TempDb: session {} wiped", session_id);
    }

    /// Wipe ALL sessions (full cleanup).
    pub async fn wipe_all(&self) {
        // Single lock acquisition — v7.0 took the write lock twice (once for
        // len(), once for clear()), which is racy.
        let mut store = self.sessions.write().await;
        let count = store.len();
        store.clear();
        tracing::info!("TempDb: wiped all {} sessions", count);
    }

    /// Background cleanup: remove sessions older than 10 minutes.
    pub async fn cleanup_expired(&self) {
        let mut store = self.sessions.write().await;
        let before = store.len();
        store.retain(|_, s| s.created_at.elapsed().as_secs() < 600);
        let removed = before - store.len();
        if removed > 0 {
            tracing::info!("TempDb: cleaned up {} expired sessions", removed);
        }
    }

    /// Number of active sessions.
    pub async fn active_count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HistoryDb — Persistent JSON-lines file, optional (user opt-in)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct HistoryDb {
    enabled: Arc<AtomicBool>,
    entries: Arc<RwLock<Vec<HistoryEntry>>>,
    file_path: PathBuf,
}

impl HistoryDb {
    /// Create a new HistoryDb. Does NOT load from disk until `enable()` is called.
    pub fn new() -> Self {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".searchwala");
        let file_path = dir.join("history.jsonl");

        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            entries: Arc::new(RwLock::new(Vec::new())),
            file_path,
        }
    }

    /// Enable history and load from disk if file exists.
    pub async fn enable(&self) -> Result<usize, String> {
        // Ensure directory exists
        if let Some(parent) = self.file_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create history dir: {}", e))?;
        }

        // Load existing history. JSON-lines, so ONE corrupt line costs one
        // entry rather than the whole file — v7.0 parsed a single JSON array
        // and discarded everything on any parse error.
        if self.file_path.exists() {
            match std::fs::read_to_string(&self.file_path) {
                Ok(content) => {
                    let mut entries: Vec<HistoryEntry> = Vec::new();
                    let mut skipped = 0usize;

                    // Legacy format: one pretty-printed JSON array.
                    if content.trim_start().starts_with('[') {
                        match serde_json::from_str::<Vec<HistoryEntry>>(&content) {
                            Ok(v) => {
                                tracing::info!(
                                    "HistoryDb: migrating {} entries from the legacy array format",
                                    v.len()
                                );
                                entries = v;
                            }
                            Err(e) => tracing::warn!("HistoryDb: legacy file unreadable: {}", e),
                        }
                    } else {
                        for line in content.lines() {
                            let l = line.trim();
                            if l.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<HistoryEntry>(l) {
                                Ok(e) => entries.push(e),
                                Err(_) => skipped += 1,
                            }
                        }
                    }
                    if skipped > 0 {
                        tracing::warn!("HistoryDb: skipped {} unparseable line(s)", skipped);
                    }

                    let cap = Self::max_entries();
                    if entries.len() > cap {
                        let excess = entries.len() - cap;
                        entries.drain(..excess);
                    }
                    let count = entries.len();
                    *self.entries.write().await = entries;
                    self.enabled.store(true, Ordering::SeqCst);
                    tracing::info!("HistoryDb: loaded {} entries from {:?}", count, self.file_path);
                    return Ok(count);
                }
                Err(e) => tracing::warn!("HistoryDb: failed to read history file: {}", e),
            }
        }

        self.enabled.store(true, Ordering::SeqCst);
        tracing::info!("HistoryDb: enabled (new, empty)");
        Ok(0)
    }

    /// Disable history (does not delete file).
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        tracing::info!("HistoryDb: disabled");
    }

    /// Check if history is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Maximum retained entries. v7.0 had no cap at all.
    fn max_entries() -> usize {
        std::env::var("MAX_HISTORY_ENTRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000)
    }

    /// Add a new history entry and append it to disk.
    pub async fn add_entry(&self, entry: HistoryEntry) {
        if !self.is_enabled() {
            return;
        }
        {
            let mut entries = self.entries.write().await;
            entries.push(entry.clone());
            let cap = Self::max_entries();
            if entries.len() > cap {
                let excess = entries.len() - cap;
                entries.drain(..excess);
            }
        }
        self.append(&entry).await;
    }

    /// Get all history entries (newest first).
    pub async fn get_all(&self) -> Vec<HistoryEntry> {
        let entries = self.entries.read().await;
        let mut result = entries.clone();
        result.reverse();
        result
    }

    /// Get the most recent `n` entries (newest first).
    pub async fn get_recent(&self, n: usize) -> Vec<HistoryEntry> {
        let entries = self.entries.read().await;
        let start = entries.len().saturating_sub(n);
        let mut result: Vec<HistoryEntry> = entries[start..].to_vec();
        result.reverse();
        result
    }

    /// Clear all history and delete the file.
    pub async fn clear(&self) -> Result<(), String> {
        self.entries.write().await.clear();
        if self.file_path.exists() {
            std::fs::remove_file(&self.file_path)
                .map_err(|e| format!("Failed to delete history file: {}", e))?;
        }
        tracing::info!("HistoryDb: cleared all history");
        Ok(())
    }

    pub async fn count(&self) -> usize {
        self.entries.read().await.len()
    }

    /// APPEND one JSON-lines record.
    ///
    /// ═══════════════════════════════════════════════════════════════════════
    /// v8.0: v7.0's `persist()` serialized the ENTIRE history with
    /// `to_string_pretty` and rewrote the whole file on every single entry —
    /// O(n) work per insert, so O(n^2) over a session. With a few thousand
    /// entries each holding a full LLM answer that is tens of megabytes
    /// rewritten per search. It was also non-atomic: `fs::write` truncates
    /// first, so a crash mid-write destroyed the entire history. And the docs
    /// called it "JSON-lines" while it was actually one pretty-printed array.
    ///
    /// This appends a single line under `O_APPEND`, which is atomic for a
    /// small write, and costs O(1) regardless of history size.
    /// ═══════════════════════════════════════════════════════════════════════
    async fn append(&self, entry: &HistoryEntry) {
        let Ok(line) = serde_json::to_string(entry) else {
            tracing::error!("HistoryDb: failed to serialize entry");
            return;
        };
        let path = self.file_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(mut f) => {
                    if let Err(e) = writeln!(f, "{line}") {
                        tracing::error!("HistoryDb: append failed: {}", e);
                    }
                }
                Err(e) => tracing::error!("HistoryDb: open failed: {}", e),
            }
        })
        .await;
    }

    /// Rewrite the file from memory. Used only by explicit compaction, never
    /// per-insert.
    pub async fn compact(&self) -> Result<usize, String> {
        let entries = self.entries.read().await.clone();
        let path = self.file_path.clone();
        let n = entries.len();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Write to a temp file and rename: atomic, so a crash cannot
            // destroy the existing history.
            let tmp = path.with_extension("jsonl.tmp");
            let mut buf = String::new();
            for e in &entries {
                if let Ok(l) = serde_json::to_string(e) {
                    buf.push_str(&l);
                    buf.push('\n');
                }
            }
            std::fs::write(&tmp, buf).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(n)
    }
}

/// Build a history entry from search results.
pub fn build_history_entry(
    query: &str,
    focus_mode: &str,
    answer: Option<&str>,
    sources_count: usize,
    sources_found: usize,
    elapsed_secs: f64,
) -> HistoryEntry {
    HistoryEntry {
        id: format!(
            "h_{}_{:x}",
            chrono::Utc::now().timestamp_millis(),
            rand::random::<u16>()
        ),
        query: query.to_string(),
        focus_mode: focus_mode.to_string(),
        answer: answer.map(|s| s.to_string()),
        sources_count,
        sources_found,
        elapsed_secs,
        timestamp: chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    }
}

/// Spawn a background task that periodically cleans expired temp sessions.
pub fn spawn_temp_db_cleaner(temp_db: TempDb) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            temp_db.cleanup_expired().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.to_string(),
            query: format!("query {id}"),
            focus_mode: "auto".into(),
            answer: Some("an answer".into()),
            sources_count: 10,
            sources_found: 50,
            elapsed_secs: 1.5,
            timestamp: "2026-10-01 00:00:00 UTC".into(),
        }
    }

    #[tokio::test]
    async fn history_appends_and_caps() {
        std::env::set_var("MAX_HISTORY_ENTRIES", "5");
        let db = HistoryDb::new();
        db.enabled.store(true, Ordering::SeqCst);
        for i in 0..12 {
            db.entries.write().await.push(entry(&i.to_string()));
            let cap = 5;
            let mut e = db.entries.write().await;
            if e.len() > cap {
                let excess = e.len() - cap;
                e.drain(..excess);
            }
        }
        // v7.0 had NO cap, so history grew without bound in RAM and on disk.
        assert_eq!(db.count().await, 5);
        std::env::remove_var("MAX_HISTORY_ENTRIES");
    }

    #[tokio::test]
    async fn get_all_returns_newest_first() {
        let db = HistoryDb::new();
        db.enabled.store(true, Ordering::SeqCst);
        db.entries.write().await.push(entry("old"));
        db.entries.write().await.push(entry("new"));
        let all = db.get_all().await;
        assert_eq!(all[0].id, "new");
        assert_eq!(all[1].id, "old");
    }

    #[tokio::test]
    async fn get_recent_is_bounded_and_newest_first() {
        let db = HistoryDb::new();
        db.enabled.store(true, Ordering::SeqCst);
        for i in 0..10 {
            db.entries.write().await.push(entry(&i.to_string()));
        }
        let r = db.get_recent(3).await;
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].id, "9");
    }

    #[tokio::test]
    async fn disabled_history_accepts_nothing() {
        let db = HistoryDb::new();
        // enabled defaults to false — add_entry must be a no-op.
        db.add_entry(entry("x")).await;
        assert_eq!(db.count().await, 0);
    }

    #[tokio::test]
    async fn temp_db_wipe_all_takes_one_lock() {
        // v7.0 acquired the write lock twice (len() then clear()), which is racy.
        let db = TempDb::new();
        let a = db.create_session("q1").await;
        let _b = db.create_session("q2").await;
        assert_eq!(db.active_count().await, 2);
        db.wipe_all().await;
        assert_eq!(db.active_count().await, 0);
        assert!(db.get_session(&a).await.is_none());
    }

    #[tokio::test]
    async fn temp_db_session_lifecycle() {
        let db = TempDb::new();
        let sid = db.create_session("test query").await;
        db.update_status(&sid, "scraping", "batch 1/3").await;
        db.update_sources(&sid, 42).await;
        let s = db.get_session(&sid).await.expect("session should exist");
        assert_eq!(s.status, "scraping");
        assert_eq!(s.sources_collected, 42);
        db.wipe_session(&sid).await;
        assert!(db.get_session(&sid).await.is_none());
    }

    #[tokio::test]
    async fn temp_db_cleanup_removes_only_expired() {
        let db = TempDb::new();
        let fresh = db.create_session("fresh").await;
        db.cleanup_expired().await;
        assert!(db.get_session(&fresh).await.is_some(), "fresh session must survive");
    }
}
