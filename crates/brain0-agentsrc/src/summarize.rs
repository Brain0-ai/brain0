//! Per-turn decision summaries with caching, and hierarchical session summaries
//!.
//!
//! The bulk of the transcript is extracted deterministically with no LLM. The only place a
//! model is *optionally* useful is condensing a single turn's intent — always small, always
//! within context, computed **once** and cached by content-hash. A deterministic summarizer
//! is the offline default; an LLM-backed one can be plugged behind the same trait.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::event::Turn;

/// Condenses a single turn into a short decision summary.
pub trait TurnSummarizer {
    fn summarize(&self, turn: &Turn) -> String;
}

/// Offline default: takes the first line of the prompt as the intent and appends the
/// touched files. No LLM, fully deterministic.
#[derive(Debug, Default)]
pub struct DeterministicSummarizer;

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let mut s: String = text.chars().take(max_chars).collect();
        s.push('…');
        s
    }
}

impl TurnSummarizer for DeterministicSummarizer {
    fn summarize(&self, turn: &Turn) -> String {
        let intent = turn
            .prompt
            .as_deref()
            .unwrap_or("")
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let intent = truncate(intent, 200);
        let files = turn.declared_paths();
        if files.is_empty() {
            intent
        } else {
            format!("{intent} — touched: {}", files.join(", "))
        }
    }
}

/// Maximum turn summaries kept verbatim in a session summary before collapsing.
const SESSION_HEAD: usize = 10;

/// Reduce a session to a bounded summary over its **turn summaries** (never the raw
/// transcript), so it stays small even for enormous sessions.
#[must_use]
pub fn session_summary(turn_summaries: &[String]) -> String {
    if turn_summaries.len() <= SESSION_HEAD {
        turn_summaries.join("\n")
    } else {
        let head = turn_summaries[..SESSION_HEAD].join("\n");
        format!(
            "{head}\n(+{} more turns)",
            turn_summaries.len() - SESSION_HEAD
        )
    }
}

/// Per-turn summarizer backed by a configurable model provider, with a
/// **fail-safe**: if the provider is unavailable, it falls back to the deterministic
/// summary so the ingest is never blocked by a missing model.
pub struct LlmTurnSummarizer {
    provider: Box<dyn brain0_models::SummarizerProvider>,
    fallback: DeterministicSummarizer,
}

impl LlmTurnSummarizer {
    #[must_use]
    pub fn new(provider: Box<dyn brain0_models::SummarizerProvider>) -> Self {
        Self {
            provider,
            fallback: DeterministicSummarizer,
        }
    }
}

impl std::fmt::Debug for LlmTurnSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmTurnSummarizer").finish_non_exhaustive()
    }
}

/// Below this many chars, a turn with no declared changes is "trivial" — a short question or
/// acknowledgment. The deterministic summary is as good as a model's there, and skipping the
/// LLM removes a large share of the calls on real sessions.
const TRIVIAL_TURN_CHARS: usize = 240;

/// Cap on the text sent to the model. Prefill dominates local-GPU latency, and a summary only
/// needs the head of the turn; capping bounds per-call time without changing the cache key
/// (the key hashes the FULL source, so a later cap change never poisons cached entries).
const SUMMARY_INPUT_CHARS: usize = 2000;

/// First `n` chars of `s`, cut on a char boundary.
fn cap_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

impl TurnSummarizer for LlmTurnSummarizer {
    fn summarize(&self, turn: &Turn) -> String {
        let source = turn.summary_source();
        // Trivial-turn router: no declared changes + short text → the model adds nothing.
        if turn.declared_paths().is_empty() && source.chars().count() < TRIVIAL_TURN_CHARS {
            return self.fallback.summarize(turn);
        }
        match self.provider.summarize(
            brain0_models::SUMMARY_INSTRUCTION,
            cap_chars(&source, SUMMARY_INPUT_CHARS),
        ) {
            Ok(summary) if !summary.trim().is_empty() => summary,
            // Fail-safe: never block ingest because the model is missing.
            _ => self.fallback.summarize(turn),
        }
    }
}

/// Cache of input-content-hash → summary, so a summary is never recomputed.
pub trait SummaryCache {
    fn get(&self, key: &str) -> Option<String>;
    fn put(&self, key: &str, value: String);
}

/// Two-level summary cache: in-memory L1 plus an optional persistent L2 (a content-keyed
/// SQLite file OUTSIDE the index), so a full index rebuild (`rm -rf .brain0`) re-pays zero
/// LLM calls for turns whose text was already summarized — on any repo (keys are content
/// hashes, sharing is safe).
#[derive(Debug)]
pub struct PersistentSummaryCache {
    mem: InMemorySummaryCache,
    db: Option<brain0_storage::SummaryCacheDb>,
}

impl PersistentSummaryCache {
    /// Build from the environment: `BRAIN0_SUMMARY_CACHE=off` disables the persistent level,
    /// a path overrides the location, unset uses `<user cache dir>/brain0/summary-cache.db`.
    /// Any open failure degrades silently to memory-only — the cache never blocks ingest.
    #[must_use]
    pub fn from_env() -> Self {
        let configured = std::env::var("BRAIN0_SUMMARY_CACHE").ok();
        let path = match configured.as_deref() {
            Some("off") => None,
            Some(p) => Some(std::path::PathBuf::from(p)),
            None => dirs::cache_dir().map(|d| d.join("brain0/summary-cache.db")),
        };
        Self::with_db(path.and_then(|p| brain0_storage::SummaryCacheDb::open(p).ok()))
    }

    /// Explicit constructor (tests; `None` = memory-only).
    #[must_use]
    pub fn with_db(db: Option<brain0_storage::SummaryCacheDb>) -> Self {
        Self {
            mem: InMemorySummaryCache::new(),
            db,
        }
    }
}

impl SummaryCache for PersistentSummaryCache {
    fn get(&self, key: &str) -> Option<String> {
        if let Some(hit) = self.mem.get(key) {
            return Some(hit);
        }
        let hit = self.db.as_ref().and_then(|db| db.get(key).ok().flatten());
        if let Some(v) = &hit {
            self.mem.put(key, v.clone()); // promote to L1
        }
        hit
    }
    fn put(&self, key: &str, value: String) {
        if let Some(db) = &self.db {
            let _ = db.put(key, &value); // write-through; failures never block ingest
        }
        self.mem.put(key, value);
    }
}

/// In-memory cache (per run) — the L1 of [`PersistentSummaryCache`], also usable alone.
#[derive(Debug, Default)]
pub struct InMemorySummaryCache {
    map: Mutex<HashMap<String, String>>,
}

impl InMemorySummaryCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SummaryCache for InMemorySummaryCache {
    fn get(&self, key: &str) -> Option<String> {
        self.map.lock().expect("cache mutex").get(key).cloned()
    }
    fn put(&self, key: &str, value: String) {
        self.map
            .lock()
            .expect("cache mutex")
            .insert(key.to_owned(), value);
    }
}

/// Content-hash key of a turn's summary source (prompt + assistant text).
#[must_use]
pub fn content_key(turn: &Turn) -> String {
    blake3::hash(turn.summary_source().as_bytes())
        .to_hex()
        .as_str()[..32]
        .to_owned()
}

/// Summarize a turn, using the cache to avoid recomputation for identical content.
pub fn summarize_cached(
    summarizer: &dyn TurnSummarizer,
    turn: &Turn,
    cache: &dyn SummaryCache,
) -> String {
    let key = content_key(turn);
    if let Some(cached) = cache.get(&key) {
        return cached;
    }
    let summary = summarizer.summarize(turn);
    cache.put(&key, summary.clone());
    summary
}

#[cfg(test)]
pub(crate) mod tests_support {
    use crate::event::{Provenance, ToolCall, Turn};
    use std::path::PathBuf;

    /// A minimal turn with the given prompt and declared paths (shared by test modules).
    pub(crate) fn turn_with(prompt: &str, paths: &[&str]) -> Turn {
        use brain0_model::chrono::TimeZone;
        Turn {
            session_id: "s".into(),
            cwd: PathBuf::from("/p"),
            timestamp: brain0_model::chrono::Utc
                .with_ymd_and_hms(2026, 7, 2, 8, 0, 0)
                .unwrap(),
            ordinal: 0,
            prompt: Some(prompt.to_owned()),
            assistant_text: String::new(),
            tool_calls: paths
                .iter()
                .map(|p| ToolCall {
                    name: "Edit".into(),
                    declared_paths: vec![(*p).into()],
                    read_paths: vec![],
                    command: None,
                })
                .collect(),
            model: None,
            provenance: Provenance {
                adapter: "codex".into(),
                file: "f".into(),
                byte_offset: 0,
            },
            read_contents: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Provenance, ToolCall};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn turn(prompt: &str, paths: &[&str]) -> Turn {
        use chrono::TimeZone;
        Turn {
            session_id: "s".into(),
            cwd: PathBuf::from("/p"),
            timestamp: chrono::Utc.timestamp_opt(0, 0).single().unwrap(),
            ordinal: 0,
            prompt: Some(prompt.into()),
            assistant_text: "did stuff".into(),
            tool_calls: paths
                .iter()
                .map(|p| ToolCall {
                    name: "Edit".into(),
                    declared_paths: vec![(*p).into()],
                    read_paths: vec![],
                    command: None,
                })
                .collect(),
            model: None,
            provenance: Provenance {
                adapter: "codex".into(),
                file: "f".into(),
                byte_offset: 0,
            },
            read_contents: Vec::new(),
        }
    }

    #[test]
    fn deterministic_summary_has_intent_and_files() {
        let s = DeterministicSummarizer
            .summarize(&turn("fix the parser\nmore detail", &["a.py", "b.py"]));
        assert!(s.starts_with("fix the parser"));
        assert!(s.contains("a.py") && s.contains("b.py"));
    }

    #[test]
    fn session_summary_is_bounded() {
        let many: Vec<String> = (0..25).map(|i| format!("turn {i}")).collect();
        let summary = session_summary(&many);
        assert!(summary.contains("(+15 more turns)"));
    }

    #[test]
    fn summary_is_cached_and_not_recomputed() {
        struct Counting(AtomicUsize);
        impl TurnSummarizer for Counting {
            fn summarize(&self, _turn: &Turn) -> String {
                self.0.fetch_add(1, Ordering::SeqCst);
                "summary".into()
            }
        }
        let counting = Counting(AtomicUsize::new(0));
        let cache = InMemorySummaryCache::new();
        let t = turn("same content", &[]);
        let a = summarize_cached(&counting, &t, &cache);
        let b = summarize_cached(&counting, &t, &cache);
        assert_eq!(a, b);
        assert_eq!(
            counting.0.load(Ordering::SeqCst),
            1,
            "summarized once, then cached"
        );
    }
}

#[cfg(test)]
mod i5_tests {
    use super::*;
    use crate::summarize::tests_support::turn_with;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Provider that counts calls and records the last input it was given.
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
        last_input: Arc<Mutex<String>>,
    }
    impl brain0_models::SummarizerProvider for CountingProvider {
        fn summarize(&self, _instruction: &str, input: &str) -> brain0_models::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_input.lock().unwrap() = input.to_owned();
            Ok("model summary".into())
        }
        fn model_id(&self) -> &str {
            "counting"
        }
    }

    fn counting() -> (LlmTurnSummarizer, Arc<AtomicUsize>, Arc<Mutex<String>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(String::new()));
        let provider = CountingProvider {
            calls: Arc::clone(&calls),
            last_input: Arc::clone(&last),
        };
        (LlmTurnSummarizer::new(Box::new(provider)), calls, last)
    }

    #[test]
    fn trivial_turns_skip_the_model_entirely() {
        let (s, calls, _) = counting();
        // Short + no declared paths → deterministic, zero LLM calls.
        let out = s.summarize(&turn_with("ok thanks", &[]));
        assert!(!out.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Short but WITH a declared change → still a real edit, the model runs.
        s.summarize(&turn_with("fix it", &["src/a.rs"]));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn model_input_is_capped_at_the_char_budget() {
        let (s, _, last) = counting();
        let long = "è".repeat(5000); // multibyte: the cap must cut on a char boundary
        s.summarize(&turn_with(&long, &["src/a.rs"]));
        let sent = last.lock().unwrap().clone();
        assert!(
            sent.chars().count() <= 2000,
            "sent {} chars",
            sent.chars().count()
        );
    }

    #[test]
    fn persistent_cache_survives_a_fresh_run() {
        let dir = std::env::temp_dir().join(format!("brain0-pcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cache.db");
        let t = turn_with("rework the tokenizer end to end", &["src/lex.rs"]);

        // Run 1: cold — one model call, written through to the db.
        let (s1, c1, _) = counting();
        let cache1 = PersistentSummaryCache::with_db(Some(
            brain0_storage::SummaryCacheDb::open(&path).unwrap(),
        ));
        let out1 = summarize_cached(&s1, &t, &cache1);
        assert_eq!(c1.load(Ordering::SeqCst), 1);

        // Run 2: a brand-new process/cache (fresh L1) sharing the db — zero model calls.
        let (s2, c2, _) = counting();
        let cache2 = PersistentSummaryCache::with_db(Some(
            brain0_storage::SummaryCacheDb::open(&path).unwrap(),
        ));
        let out2 = summarize_cached(&s2, &t, &cache2);
        assert_eq!(
            c2.load(Ordering::SeqCst),
            0,
            "must hit the persistent cache"
        );
        assert_eq!(out1, out2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
