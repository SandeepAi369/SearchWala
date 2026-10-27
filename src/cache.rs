// ============================================================================
// TempDb — in-memory state of searches in progress. Auto-wipes when a task
// finishes. Saved conversations live in `chats.rs` (SQLite).
// ============================================================================

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

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

impl Default for TempDb {
    fn default() -> Self {
        Self::new()
    }
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
