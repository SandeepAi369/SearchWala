// ============================================================================
// SearchWala v8.0 — library target
//
// Exists so `src/bin/eval.rs` can drive the real pipeline. v7.0 was a bin-only
// crate, which is part of why `eval.rs` and the 50-query golden set were never
// wired up: nothing could import them.
// ============================================================================

pub mod cache;
pub mod cache_layer;
pub mod config;
pub mod copilot;
pub mod engines;
pub mod fetcher;
pub mod host_gate;
pub mod eval;
pub mod extractor;
pub mod llm;
pub mod models;
pub mod query_intel;
pub mod ranking;
pub mod search;
pub mod ssrf;
pub mod url_utils;
pub mod stream;
#[cfg(test)]
pub mod testalloc;
pub mod proxy_pool;
pub mod util;


/// Test-only counting allocator, so leak regressions can assert on live
/// allocated bytes instead of process RSS (which is polluted by the parallel
/// test harness).
#[cfg(test)]
#[global_allocator]
static TEST_ALLOC: testalloc::Counting = testalloc::Counting;
