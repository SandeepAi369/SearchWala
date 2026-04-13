// ============================================================================
// Qrux v5.0.1 — Dual Database System
//
// 1. TempDb   — In-memory HashMap for active search sessions.
//               Auto-wipes completely when a task finishes.
//
// 2. HistoryDb — Optional persistent JSON-lines file (~/.qrux/history.json).
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
        let count = self.sessions.write().await.len();
        self.sessions.write().await.clear();
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
        let dir = PathBuf::from(home).join(".qrux");
        let file_path = dir.join("history.json");

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

        // Load existing history
        if self.file_path.exists() {
            match std::fs::read_to_string(&self.file_path) {
                Ok(content) => {
                    match serde_json::from_str::<Vec<HistoryEntry>>(&content) {
                        Ok(entries) => {
                            let count = entries.len();
                            *self.entries.write().await = entries;
                            self.enabled.store(true, Ordering::SeqCst);
                            tracing::info!("HistoryDb: loaded {} entries from {:?}", count, self.file_path);
                            return Ok(count);
                        }
                        Err(e) => {
                            tracing::warn!("HistoryDb: failed to parse history file: {}", e);
                            // Start fresh
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("HistoryDb: failed to read history file: {}", e);
                }
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

    /// Add a new history entry and persist to disk.
    pub async fn add_entry(&self, entry: HistoryEntry) {
        if !self.is_enabled() {
            return;
        }

        {
            let mut entries = self.entries.write().await;
            entries.push(entry);
        }

        // Persist asynchronously
        self.persist().await;
    }

    /// Get all history entries (newest first).
    pub async fn get_all(&self) -> Vec<HistoryEntry> {
        let entries = self.entries.read().await;
        let mut result = entries.clone();
        result.reverse();
        result
    }

    /// Get recent N entries.
    pub async fn get_recent(&self, n: usize) -> Vec<HistoryEntry> {
        let entries = self.entries.read().await;
        let start = entries.len().saturating_sub(n);
        let mut result: Vec<HistoryEntry> = entries[start..].to_vec();
        result.reverse();
        result
    }

    /// Clear all history and delete file.
    pub async fn clear(&self) -> Result<(), String> {
        self.entries.write().await.clear();
        if self.file_path.exists() {
            std::fs::remove_file(&self.file_path)
                .map_err(|e| format!("Failed to delete history file: {}", e))?;
        }
        tracing::info!("HistoryDb: cleared all history");
        Ok(())
    }

    /// Get entry count.
    pub async fn count(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Persist entries to JSON file.
    async fn persist(&self) {
        let entries = self.entries.read().await.clone();
        let path = self.file_path.clone();

        // Do file I/O in a blocking task to not block the async runtime
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(&entries) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::error!("HistoryDb: failed to write: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("HistoryDb: failed to serialize: {}", e);
                }
            }
        })
        .await
        .ok();
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
