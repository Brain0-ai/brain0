//! `brain0` — the command-line entry point.
//!
//! brain0 never writes to the observed repository. It reads git (the *fact* side) and
//! passively reads coding-agent transcripts + memory (the *declared/intent* side), and
//! writes only to its own index + payload store. MCP is offered as a **query** channel.

mod mcp_query;
mod query;
mod report;
mod rewind;
mod today;

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use brain0_agentsrc::{
    discover, resolve_project_root, run_ingest_reporting, DeterministicSummarizer, DiscoveryConfig,
    IngestProgress, LlmTurnSummarizer, ProjectScope, RedactionConfig, Redactor, TurnSummarizer,
};
use brain0_crypto::{EnvKeyProvider, FileKeyProvider};
use brain0_identity::IdentityConfig;
use brain0_mcp::JsonRpcServer;
use brain0_model::Level;
use brain0_models::{
    build_embedder, build_summarizer, EmbeddingProvider, LocalEmbeddingProvider, ModelConfig,
    SUMMARY_INSTRUCTION,
};
use brain0_observer::{ingest, CheckpointEngine, GitReader, Snapshot};
use brain0_policy::{evaluate, Policy, ReadEvent, Severity};
use brain0_risk::{
    apply_aposteriori, derive_aposteriori_factors, recompute_aggregates, recompute_apriori,
    recompute_cochange_coupling, AprioriContext, CoChangeConfig,
};
use brain0_storage::{
    verify_payload, EncryptedPayloadStore, FsPayloadStore, IntegrityStatus, PayloadStore,
    SqliteStorage, Storage,
};
use clap::{Parser, Subcommand};

/// Environment variable carrying the KEK (64 hex chars) for payload encryption.
const ENV_KEK: &str = "BRAIN0_KEK";

#[derive(Parser)]
#[command(
    name = "brain0",
    version,
    about = "Passive decision-graph for AI-assisted coding"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Observe a repo's git history (or a filesystem checkpoint) into the index (the FACT side).
    Ingest {
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        #[arg(long, default_value = "local")]
        author: String,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        /// Store payload unencrypted (e.g. on a full-disk-encrypted machine).
        #[arg(long)]
        no_encrypt_payload: bool,
    },
    /// Passively ingest coding-agent transcripts + memory for the current project (DECLARED side).
    Observe {
        /// Observed repository id to reconcile against (matches `ingest --repo`); optional.
        #[arg(long)]
        repo: Option<String>,
        /// Project path used for scoping (defaults to the current directory's project root).
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        /// Observe every project's sessions, not only the current one.
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        #[arg(long)]
        no_encrypt_payload: bool,
    },
    /// Root-cause debug query against the index (by reference).
    Query {
        text: String,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value_t = 8)]
        k: usize,
    },
    /// Accountability report: drift, sensitive reads (DLP), top risk, agent footprint.
    Report {
        /// Repo id (defaults to the only repo in the index; required when there are several).
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        /// Emit markdown instead of terminal text.
        #[arg(long)]
        md: bool,
        /// How many top-risk files to list.
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Morning triage: what agents and humans did in the last window, attention first.
    Today {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        /// Window size: `24h`, `7d`, `90m` (bare numbers mean hours).
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Restore the working tree from a recorded checkpoint (the `watch` safety net).
    Rewind {
        /// Checkpoint id to restore (see --list). A fresh pre-rewind checkpoint is recorded
        /// first, so a rewind is itself reversible.
        #[arg(long)]
        to: Option<String>,
        /// List restorable checkpoints and exit.
        #[arg(long)]
        list: bool,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        #[arg(long)]
        no_encrypt_payload: bool,
    },
    /// Serve the MCP query channel over stdio (debug/audit tools for an external agent).
    Mcp {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Verify content-addressed integrity of all stored payloads.
    Verify {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        #[arg(long)]
        no_encrypt_payload: bool,
    },
    /// Show the recent security audit log (redactions, purges, …).
    Audit {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// DLP egress guard: flag sensitive/secret files an agent READ that reached a remote model.
    Guard {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        /// Exit non-zero if any CRITICAL violation is found (for CI gating).
        #[arg(long)]
        strict: bool,
        /// Run as a live daemon: poll the index and stream NEW violations to an alert sink.
        #[arg(long)]
        watch: bool,
        /// (--watch) Poll interval in seconds.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// (--watch) POST each alert as JSON to this URL.
        #[arg(long)]
        webhook: Option<String>,
        /// (--watch) POST each alert to a Slack incoming-webhook URL.
        #[arg(long)]
        slack: Option<String>,
        /// (--watch) Minimum severity to alert on (info|warn|critical).
        #[arg(long, default_value = "critical")]
        min_severity: String,
        /// (--watch) Run a single poll tick and exit (for CI / testing).
        #[arg(long)]
        once: bool,
    },
    /// Preflight gate: scan files (or --staged) for secrets/sensitive paths before they reach a
    /// remote model. Exits non-zero on a critical — wire as a git pre-commit or agent pre-run hook.
    Preflight {
        /// Files to check (omit and use --staged for the git index).
        files: Vec<String>,
        /// Check the git staging area (`git diff --cached`) instead of / in addition to FILES.
        #[arg(long)]
        staged: bool,
        /// Report only; never exit non-zero (advisory mode).
        #[arg(long)]
        warn_only: bool,
    },
    /// AI provenance for a commit (by SHA): which agent/model produced it, files changed, reads,
    /// secrets read, and declared↔done drift — printed as JSON.
    Provenance {
        /// Commit SHA (full or a prefix).
        commit: String,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
    },
    /// Emit a signed (Ed25519) in-toto AI-provenance attestation for a commit.
    Attest {
        /// Commit SHA (full or a prefix).
        commit: String,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        /// Signing-key file (generated 0600 if absent); or set BRAIN0_ATTEST_KEY (hex seed).
        #[arg(long, default_value = ".brain0/attest.key")]
        key_file: PathBuf,
    },
    /// Verify a signed attestation envelope (file). Pass --pubkey to check a trusted key.
    VerifyAttestation {
        /// Path to the attestation envelope JSON.
        file: PathBuf,
        /// Trusted public key (hex). Defaults to the envelope's embedded key.
        #[arg(long)]
        pubkey: Option<String>,
    },
    /// Per-file / per-hunk attribution for a commit: changed line ranges + the model(s) behind it.
    Attribution {
        /// Commit SHA (full or a prefix).
        commit: String,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        #[arg(long)]
        no_encrypt_payload: bool,
    },
    /// Compliance pack: aggregate AI-provenance across all commits for auditors.
    Compliance {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        /// Emit the full report as JSON.
        #[arg(long)]
        json: bool,
        /// Exit non-zero if any secret-bearing read is found (CI gate).
        #[arg(long)]
        strict: bool,
    },
    /// Re-embed the whole corpus (after changing the embedding model/dimension).
    Reembed {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        #[arg(long)]
        no_encrypt_payload: bool,
    },
    /// Purge / crypto-shred payload (keeping the graph topology) by task or by age.
    Purge {
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        #[arg(long)]
        no_encrypt_payload: bool,
        /// Purge a specific task's payload.
        #[arg(long)]
        task: Option<String>,
        /// Retention: purge payload of task versions older than N days.
        #[arg(long)]
        older_than_days: Option<i64>,
    },
    /// Watch a repo (no-git) and append a checkpoint on every debounced change.
    Watch {
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = ".brain0/index.db")]
        db: PathBuf,
        #[arg(long, default_value = ".brain0/payload")]
        payload: PathBuf,
        #[arg(long, default_value = "local")]
        author: String,
        #[arg(long, default_value_t = 1500)]
        debounce_ms: u64,
        #[arg(long, default_value = ".brain0/keyring.key")]
        key_file: PathBuf,
        #[arg(long)]
        no_encrypt_payload: bool,
    },
}

fn open_storage(db: &Path) -> Result<SqliteStorage> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    SqliteStorage::open(db).context("opening index database")
}

/// Open the payload store. Encrypted by default (secure-by-default): the KEK comes from
/// `BRAIN0_KEK` if set, else an auto-generated 0600 key file. `--no-encrypt-payload` opts
/// out (e.g. on a full-disk-encrypted machine).
fn open_payload(dir: &Path, encrypt: bool, key_file: &Path) -> Result<Box<dyn PayloadStore>> {
    if !encrypt {
        eprintln!("warning: payload encryption disabled (--no-encrypt-payload)");
        return Ok(Box::new(
            FsPayloadStore::open(dir).context("opening payload store")?,
        ));
    }
    if std::env::var(ENV_KEK).is_ok() {
        Ok(Box::new(
            EncryptedPayloadStore::open(dir, EnvKeyProvider::new("env", ENV_KEK))
                .context("opening encrypted payload store (BRAIN0_KEK)")?,
        ))
    } else {
        if let Some(parent) = key_file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Ok(Box::new(
            EncryptedPayloadStore::open(
                dir,
                FileKeyProvider::new("file", key_file.to_path_buf(), true),
            )
            .context("opening encrypted payload store (key file)")?,
        ))
    }
}

/// Build the configured embedder, falling back to the offline local embedder if a remote
/// provider (e.g. Ollama) is unreachable — so brain0 stays usable air-gapped.
fn embedder_with_fallback(config: &brain0_models::EmbeddingConfig) -> Box<dyn EmbeddingProvider> {
    let embedder = build_embedder(config);
    let err = match embedder.embed("brain0 health check") {
        Ok(_) => return embedder,
        Err(err) => err,
    };
    // Say WHY (a missing model reads very differently from a down server), and how to fix it.
    eprintln!(
        "warning: embedding model '{}' failed: {err}\n  hint: ollama pull {}",
        config.model, config.model
    );
    // Documented fallback chain: try the widely-installed nomic model before going
    // offline — real semantic vectors beat feature-hashing whenever a server is up.
    if config.provider != "local" && config.model != "nomic-embed-text" {
        let nomic = brain0_models::OllamaEmbeddingProvider::new(
            "nomic-embed-text",
            config.endpoint.clone(),
            768,
        );
        if nomic.embed("brain0 health check").is_ok() {
            eprintln!("  falling back to 'nomic-embed-text' (dim 768)");
            return Box::new(nomic);
        }
    }
    eprintln!("  falling back to local feature-hash embeddings");
    Box::new(LocalEmbeddingProvider::new(brain0_storage::LOCAL_EMBED_DIM))
}

/// Read a `usize` from an environment variable, falling back to `default` if unset/unparseable.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Normalized diff size (0..=1, soft knee at 50 lines) of an artifact's latest change.
fn latest_diff_size(
    storage: &SqliteStorage,
    artifact_id: &brain0_model::ArtifactId,
) -> Result<f32> {
    Ok(storage
        .artifact_versions(artifact_id)?
        .last()
        .map(|v| {
            let lines = (v.lines_added + v.lines_removed) as f32;
            lines / (lines + 50.0)
        })
        .unwrap_or(0.0))
}

/// Map each "changed but not declared" path to its strongest reconciliation drift score, gathered
/// across every task version. This turns declared↔done drift into a per-artifact a-priori signal
/// (a surprise change the agent never declared) instead of leaving the drift dimension dead.
fn collect_undeclared_drift(
    storage: &SqliteStorage,
) -> Result<std::collections::HashMap<String, f32>> {
    let mut map: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
    for task_id in storage.all_task_ids()? {
        for v in storage.task_versions(&task_id)? {
            if let Some(drift) = &v.drift {
                for path in &drift.undeclared {
                    let entry = map.entry(path.clone()).or_insert(0.0);
                    *entry = entry.max(drift.score);
                }
            }
        }
    }
    Ok(map)
}

/// Recompute both risk scores for every symbol and file, then derive aggregates bottom-up.
///
/// a-priori (per artifact): churn (version count), latest diff size, and declared↔done drift.
/// centrality/blast stay 0 until a dependency graph is extracted, but the noisy-OR scorer means
/// absent signals no longer cap the score. a-posteriori: revert + immediate-fix derived from the
/// artifact's own version history (no external evidence here). Files are scored directly too, so
/// files with no parsed symbols (e.g. Rust) are not stuck at zero.
fn recompute_risk(storage: &SqliteStorage, repo: &str) -> Result<()> {
    // First derive the language-agnostic co-change graph from commit history so centrality and
    // blast radius have edges to read (overridable for small/large histories).
    let cochange = CoChangeConfig {
        min_support: env_usize(
            "BRAIN0_COCHANGE_MIN_SUPPORT",
            CoChangeConfig::default().min_support,
        ),
        max_commit_files: env_usize(
            "BRAIN0_COCHANGE_MAX_COMMIT_FILES",
            CoChangeConfig::default().max_commit_files,
        ),
    };
    recompute_cochange_coupling(storage, repo, &cochange)?;

    let undeclared = collect_undeclared_drift(storage)?;
    // An "immediate fix" is a fix-shaped follow-up landing within this window (see brain0-risk).
    let fix_window = chrono::Duration::minutes(30);
    // Churn recency horizon: the repo's latest observed timestamp (deterministic, not wall clock).
    let now = latest_observed_timestamp(storage, repo)?;
    for level in [Level::Symbol, Level::File] {
        for art in storage.list_artifacts(repo, level)? {
            let ctx = AprioriContext {
                diff_size: latest_diff_size(storage, &art.id)?,
                // Test coverage is not measured yet → no penalty (a constant 0.5 used to flatten
                // every score; feeding 0.0 keeps the signal neutral until coverage is wired).
                test_gap: 0.0,
                drift: undeclared.get(&art.qualified_path).copied().unwrap_or(0.0),
                now,
            };
            recompute_apriori(storage, &art.id, &ctx)?;
            let post = derive_aposteriori_factors(storage, &art.id, fix_window, false, false)?;
            apply_aposteriori(storage, &art.id, &post)?;
        }
    }
    recompute_aggregates(storage, repo)?;
    Ok(())
}

/// The repo's newest observed version timestamp — the deterministic "now" for churn decay.
fn latest_observed_timestamp(
    storage: &SqliteStorage,
    repo: &str,
) -> Result<brain0_model::Timestamp> {
    let mut latest = brain0_model::chrono::DateTime::<brain0_model::chrono::Utc>::UNIX_EPOCH;
    for file in storage.list_artifacts(repo, Level::File)? {
        for v in storage.artifact_versions(&file.id)? {
            if v.timestamp > latest {
                latest = v.timestamp;
            }
        }
    }
    Ok(latest)
}

fn ingest_snapshot(
    storage: &SqliteStorage,
    payload: &dyn PayloadStore,
    snapshot: &Snapshot,
) -> Result<()> {
    let report = ingest(storage, payload, snapshot, &IdentityConfig::default())?;
    println!(
        "  · {} changed (added {}, modified {}, renamed {}, moved {}, removed {})",
        report.changed_artifacts,
        report.added,
        report.modified,
        report.renamed,
        report.moved,
        report.removed
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_ingest(
    repo: &str,
    path: &Path,
    db: &Path,
    payload: &Path,
    author: &str,
    key_file: &Path,
    encrypt: bool,
) -> Result<()> {
    let storage = open_storage(db)?;
    let payload_store = open_payload(payload, encrypt, key_file)?;
    // Persist the at-rest mode so downstream consumers (e.g. the GUI refresh) never downgrade
    // an encrypted store to plaintext. Idempotent; does not change command output.
    storage.set_payload_encryption(encrypt)?;
    match GitReader::open(path, repo) {
        Ok(reader) => {
            let snapshots = reader.snapshots_since(None)?;
            println!("git mode: {} commit(s)", snapshots.len());
            for snapshot in &snapshots {
                ingest_snapshot(&storage, payload_store.as_ref(), snapshot)?;
            }
        }
        Err(_) => {
            println!("no-git mode: one filesystem checkpoint");
            let snapshot = brain0_observer::snapshot_directory(
                path,
                repo,
                brain0_model::Author::new(author),
                brain0_model::Agent::human(),
                chrono::Utc::now(),
            )?;
            ingest_snapshot(&storage, payload_store.as_ref(), &snapshot)?;
        }
    }
    recompute_risk(&storage, repo)?;
    println!("done. index: {}", db.display());
    Ok(())
}

/// Format a duration in seconds as a compact human ETA (`1h02m`, `3m07s`, `12s`).
fn fmt_eta(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// A progress reporter that renders a single, in-place updating line on stderr with a percentage,
/// throughput, and ETA — so a long `observe`/`ingest` pass (per-turn model summarization) is not
/// silent. stdout stays clean for the final machine-readable stats line.
struct CliProgress {
    start: Instant,
    last_render: Cell<Instant>,
}

impl CliProgress {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last_render: Cell::new(now),
        }
    }
}

impl IngestProgress for CliProgress {
    fn on_plan(&self, sessions: usize, turns: usize) {
        if turns == 0 {
            eprintln!("up to date: no new turns to process");
        } else {
            eprintln!("processing {sessions} session(s), {turns} new turn(s)…");
        }
    }

    fn on_turn_start(&self, starting: usize, total: usize) {
        if total == 0 {
            return;
        }
        // Heartbeat right before the slow per-turn summarization, so the line is never silent
        // while a single (possibly multi-second) model call is in flight.
        let elapsed = self.start.elapsed().as_secs_f64();
        let pct = ((starting.saturating_sub(1)) as f64 / total as f64) * 100.0;
        eprint!(
            "\r  {starting}/{total} turns ({pct:5.1}%)  summarizing…  elapsed {}        ",
            fmt_eta(elapsed)
        );
        let _ = std::io::stderr().flush();
    }

    fn on_turn(&self, done: usize, total: usize) {
        if total == 0 {
            return;
        }
        // Throttle redraws to ~10/s, but always render the final turn.
        let now = Instant::now();
        if done < total && now.duration_since(self.last_render.get()).as_millis() < 100 {
            return;
        }
        self.last_render.set(now);

        let elapsed = self.start.elapsed().as_secs_f64();
        let pct = (done as f64 / total as f64) * 100.0;
        let rate = if elapsed > 0.0 {
            done as f64 / elapsed
        } else {
            0.0
        };
        let eta = if done > 0 {
            elapsed / done as f64 * (total - done) as f64
        } else {
            0.0
        };
        eprint!(
            "\r  {done}/{total} turns ({pct:5.1}%)  {rate:.2} turn/s  ETA {}    ",
            fmt_eta(eta)
        );
        let _ = std::io::stderr().flush();
    }

    fn on_done(&self) {
        // Terminate the in-place progress line.
        eprintln!();
    }
}

fn cmd_observe(
    repo: Option<&str>,
    path: &Path,
    db: &Path,
    payload: &Path,
    all: bool,
    key_file: &Path,
    encrypt: bool,
) -> Result<()> {
    let storage = open_storage(db)?;
    let payload_store = open_payload(payload, encrypt, key_file)?;
    // Persist the at-rest mode so downstream consumers (e.g. the GUI refresh) never downgrade
    // an encrypted store to plaintext. Idempotent; does not change command output.
    storage.set_payload_encryption(encrypt)?;
    let registry = discover(&DiscoveryConfig::default());
    if registry.is_empty() {
        println!("no agent artifact sources discovered (set BRAIN0_AGENT_ROOTS to add some)");
        return Ok(());
    }
    let scope = if all {
        ProjectScope::All
    } else {
        ProjectScope::Project(resolve_project_root(path))
    };
    let redactor = Redactor::new(&RedactionConfig::from_env())
        .context("compiling redaction config from BRAIN0_EXCLUDE/BRAIN0_REDACT")?;
    let config = ModelConfig::load(None);

    // Announce configured models up front (immediate output), then probe. Inference itself runs
    // in the model server (e.g. Ollama), which uses the GPU (CUDA/Metal/ROCm) automatically when
    // available; brain0 only calls it over HTTP. The probe can be slow on a cold/large model, so
    // we say so before blocking — never a silent gap.
    eprintln!("models (configured):");
    eprintln!(
        "  summarizer: {} · {} @ {}",
        config.summarizer.provider, config.summarizer.model, config.summarizer.endpoint
    );
    eprintln!(
        "  embeddings: {} · {} @ {} (dim {})",
        config.embedding.provider,
        config.embedding.model,
        config.embedding.endpoint,
        config.embedding.dim
    );

    // The probe is LAZY (first real model call): a run fully served by the persistent summary
    // cache (or by the trivial-turn router) never loads the model at all — rebuilds cost seconds.
    // The fail-safe stands: an unreachable model falls back to deterministic on first use.
    let turn_summarizer: Box<dyn TurnSummarizer> = if config.summarizer.provider == "deterministic"
    {
        eprintln!("  summarizer in use: deterministic (offline, no model server)");
        Box::new(DeterministicSummarizer)
    } else {
        eprintln!(
            "  summarizer: {} (lazy — loads on the first uncached turn)",
            config.summarizer.model
        );
        Box::new(LazyProbeSummarizer::new(config.summarizer.clone()))
    };

    eprintln!("  probing embedder '{}'…", config.embedding.model);
    let _ = std::io::stderr().flush();
    let embedder = embedder_with_fallback(&config.embedding);
    eprintln!(
        "  embeddings in use: {} (dim {}) ✓",
        embedder.model_id(),
        embedder.dim()
    );

    let progress = CliProgress::new();
    let stats = run_ingest_reporting(
        &registry,
        &scope,
        repo,
        &storage,
        payload_store.as_ref(),
        turn_summarizer.as_ref(),
        embedder.as_ref(),
        &redactor,
        &progress,
    )?;
    println!(
        "observed {} session(s), {} turn(s) → {} task(s), {} embedding(s)",
        stats.sessions, stats.turns, stats.tasks, stats.embeddings
    );
    // Risk lives on artifacts (the FACT side), but drift is produced by this DECLARED-side pass.
    // With a repo to scope, recompute so the freshly reconciled drift feeds the a-priori score.
    if let Some(repo) = repo {
        recompute_risk(&storage, repo)?;
    }
    Ok(())
}

fn cmd_query(text: &str, db: &Path, k: usize) -> Result<()> {
    let storage = open_storage(db)?;
    let embedder = embedder_with_fallback(&ModelConfig::load(None).embedding);
    let result = query::debug(&storage, embedder.as_ref(), text, k, &chrono::Utc::now())?;
    println!("{}", result.explanation);
    println!("\ntasks:     {:?}", result.tasks);
    println!(
        "artifacts: {:?} (top {} of {})",
        result.artifacts,
        result.artifacts.len(),
        result.artifacts_total
    );
    Ok(())
}

/// A summarizer that defers probing/loading the model until the FIRST uncached turn actually
/// needs it. Runs fully served by the persistent summary cache never touch the model server.
/// The probe result is memoized; an unreachable model prints once and falls back for the run.
struct LazyProbeSummarizer {
    config: brain0_models::SummarizerConfig,
    inner: std::sync::OnceLock<Box<dyn TurnSummarizer>>,
}

impl LazyProbeSummarizer {
    fn new(config: brain0_models::SummarizerConfig) -> Self {
        Self {
            config,
            inner: std::sync::OnceLock::new(),
        }
    }
}

impl TurnSummarizer for LazyProbeSummarizer {
    fn summarize(&self, turn: &brain0_agentsrc::Turn) -> String {
        self.inner
            .get_or_init(|| {
                let provider = build_summarizer(&self.config);
                eprintln!(
                    "\n  probing summarizer '{}' (first uncached turn — may load the model)…",
                    self.config.model
                );
                let _ = std::io::stderr().flush();
                if provider.summarize(SUMMARY_INSTRUCTION, "ping").is_ok() {
                    eprintln!("  summarizer in use: {} ✓", self.config.model);
                    Box::new(LlmTurnSummarizer::new(provider))
                } else {
                    eprintln!(
                        "  summarizer '{}' unreachable → deterministic offline fallback",
                        self.config.model
                    );
                    Box::new(DeterministicSummarizer)
                }
            })
            .summarize(turn)
    }
}

fn cmd_report(repo: Option<&str>, db: &Path, md: bool, top: usize) -> Result<()> {
    let storage = open_storage(db)?;
    // Default to the only repo in the index; with several, the user must pick one.
    let repo = match repo {
        Some(r) => r.to_owned(),
        None => {
            let repos = storage.repos()?;
            match repos.as_slice() {
                [only] => only.clone(),
                [] => anyhow::bail!("no repo in the index — run `brain0 ingest` first"),
                many => anyhow::bail!(
                    "several repos in the index — pass --repo one of: {}",
                    many.join(", ")
                ),
            }
        }
    };
    let data = report::collect(&storage, &repo, top)?;
    print!("{}", report::render(&data, md));
    Ok(())
}

fn cmd_today(db: &Path, since: &str) -> Result<()> {
    let storage = open_storage(db)?;
    let window = today::parse_since(since)
        .with_context(|| format!("invalid --since {since:?} (use e.g. 24h, 7d, 90m)"))?;
    let now = chrono::Utc::now();
    let data = today::collect(&storage, now - window, now)?;
    print!("{}", today::render(&data));
    Ok(())
}

fn cmd_mcp(db: &Path, repo: Option<String>) -> Result<()> {
    let storage = open_storage(db)?;
    let embedder = embedder_with_fallback(&ModelConfig::load(None).embedding);
    let provider = mcp_query::QueryTools::new(&storage, embedder.as_ref(), repo);
    let server = JsonRpcServer::new(provider);
    server.serve(std::io::stdin().lock(), std::io::stdout().lock())?;
    Ok(())
}

/// Re-embedding migration: change embedding model/dimension by re-embedding the
/// whole corpus from the (already-redacted) payload, replacing the old vectors.
fn cmd_reembed(db: &Path, payload: &Path, key_file: &Path, encrypt: bool) -> Result<()> {
    let storage = open_storage(db)?;
    let store = open_payload(payload, encrypt, key_file)?;
    let config = ModelConfig::load(None);
    let embedder = embedder_with_fallback(&config.embedding);

    // Replace, never mix: clear old vectors, then declare the new model/dimension.
    storage.clear_all_embeddings()?;
    storage.set_embedding_meta(embedder.model_id(), embedder.dim())?;

    let mut reembedded = 0usize;
    let mut skipped = 0usize;
    for task_id in storage.all_task_ids()? {
        let mut parts = Vec::new();
        for reference in storage.task_payload_refs(&task_id)? {
            if let Some(text) = store.get_str(&reference)? {
                parts.push(text);
            }
        }
        let text = parts.join("\n");
        if text.trim().is_empty() {
            skipped += 1;
            continue;
        }
        match embedder.embed(&text) {
            Ok(vector) => {
                storage.put_task_embedding(&task_id, &vector)?;
                reembedded += 1;
            }
            Err(err) => {
                skipped += 1;
                eprintln!("skip {task_id}: {err}");
            }
        }
    }
    storage.append_audit(
        "reembed",
        &format!(
            "model={} dim={} reembedded={reembedded} skipped={skipped}",
            embedder.model_id(),
            embedder.dim()
        ),
        chrono::Utc::now(),
    )?;
    // Consistency: the store now declares exactly one model/dimension.
    let (model, dim) = storage
        .get_embedding_meta()?
        .ok_or_else(|| anyhow::anyhow!("embedding meta missing after migration"))?;
    println!("re-embedded {reembedded} task(s) ({skipped} skipped) with {model} (dim {dim})");
    Ok(())
}

fn cmd_verify(db: &Path, payload: &Path, key_file: &Path, encrypt: bool) -> Result<()> {
    let storage = open_storage(db)?;
    let store = open_payload(payload, encrypt, key_file)?;
    let mut ok = 0usize;
    let mut corrupt = 0usize;
    let mut missing = 0usize;
    for reference in storage.all_payload_refs()? {
        match verify_payload(store.as_ref(), &reference)? {
            IntegrityStatus::Ok => ok += 1,
            IntegrityStatus::Corrupt => {
                corrupt += 1;
                eprintln!("CORRUPT: {reference}");
            }
            IntegrityStatus::Missing => missing += 1,
        }
    }
    println!("integrity: {ok} ok, {corrupt} corrupt, {missing} missing/purged");
    if corrupt > 0 {
        anyhow::bail!("{corrupt} payload(s) failed integrity verification");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_purge(
    db: &Path,
    payload: &Path,
    key_file: &Path,
    encrypt: bool,
    task: Option<&str>,
    older_than_days: Option<i64>,
) -> Result<()> {
    use std::collections::BTreeSet;
    let storage = open_storage(db)?;
    let store = open_payload(payload, encrypt, key_file)?;

    // Collect (task_id, payload_ref) targets.
    let mut targets: Vec<(brain0_model::TaskId, brain0_model::PayloadRef)> = Vec::new();
    if let Some(task) = task {
        let tid = brain0_model::TaskId::new(task);
        for r in storage.task_payload_refs(&tid)? {
            targets.push((tid.clone(), r));
        }
    }
    if let Some(days) = older_than_days {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        targets.extend(storage.payloads_older_than(cutoff)?);
    }
    if task.is_none() && older_than_days.is_none() {
        anyhow::bail!("specify --task <id> and/or --older-than-days <n>");
    }

    let mut shredded = 0usize;
    let mut affected_tasks: BTreeSet<String> = BTreeSet::new();
    for (tid, reference) in &targets {
        if store.shred(reference)? {
            shredded += 1;
        }
        storage.mark_payload_purged(reference)?;
        affected_tasks.insert(tid.as_str().to_owned());
    }
    // Invalidate derived embeddings (a secret must not survive in the vector).
    for tid in &affected_tasks {
        storage.delete_task_embedding(&brain0_model::TaskId::new(tid.clone()))?;
    }
    storage.append_audit(
        "purge",
        &format!(
            "targets={} shredded={shredded} tasks={}",
            targets.len(),
            affected_tasks.len()
        ),
        chrono::Utc::now(),
    )?;
    println!(
        "purged {} payload(s); invalidated {} task embedding(s); topology preserved",
        shredded,
        affected_tasks.len()
    );
    Ok(())
}

/// A path is "external" (out-of-repo) when it is absolute — unix `/…` or windows `X:\…`. In-repo
/// reads were normalized to repo-relative at ingest, so absolute == outside the project.
fn is_external_path(path: &str) -> bool {
    path.starts_with('/') || (path.as_bytes().get(1) == Some(&b':') && path.len() > 2)
}

/// DLP egress guard (`docs/governance.md`): evaluate the recorded agent
/// reads against the policy and report files that reached a remote model. Reads are produced by
/// agent sessions (source_adapter set) — cloud agents (Codex / Claude Code) run REMOTE models, so
/// every recorded read reached a remote model. Writes each violation to the append-only audit log.
/// One DLP finding: a policy violation for a file a remote-model agent read.
struct GuardFinding {
    severity: Severity,
    rule: String,
    path: String,
    detail: String,
    model: String,
    task: String,
}

/// Evaluate the policy over every agent-session read in the index (the shared core of `guard` and
/// `watch`). Reads come from agent sessions (source_adapter set) — cloud agents run REMOTE models.
fn collect_findings(storage: &SqliteStorage, policy: &Policy) -> Result<Vec<GuardFinding>> {
    use std::collections::{BTreeSet, HashMap};
    let mut findings = Vec::new();
    for task_id in storage.all_task_ids()? {
        let node = storage.get_task_node(&task_id)?;
        let remote = node
            .as_ref()
            .and_then(|n| n.source_adapter.as_ref())
            .is_some();
        if !remote {
            continue; // commit/observer tasks have no reads; only agent sessions egress
        }
        let model = node
            .as_ref()
            .and_then(|n| n.model.clone())
            .unwrap_or_else(|| "unknown-model".to_owned());
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for version in storage.task_versions(&task_id)? {
            // Secret kinds detected in each read's content (from ingest-time scanning, §0.3).
            let secrets: HashMap<&str, &Vec<String>> = version
                .read_secrets
                .iter()
                .map(|rs| (rs.path.as_str(), &rs.kinds))
                .collect();
            for path in &version.reads {
                if !seen.insert(path.clone()) {
                    continue;
                }
                let event = ReadEvent {
                    path: path.clone(),
                    external: is_external_path(path),
                    secret_kinds: secrets
                        .get(path.as_str())
                        .map(|k| (*k).clone())
                        .unwrap_or_default(),
                    remote,
                };
                for violation in evaluate(policy, &event) {
                    findings.push(GuardFinding {
                        severity: violation.severity,
                        rule: violation.rule,
                        path: violation.path,
                        detail: violation.detail,
                        model: model.clone(),
                        task: task_id.as_str().to_owned(),
                    });
                }
            }
        }
    }
    Ok(findings)
}

fn cmd_guard(db: &Path, strict: bool) -> Result<()> {
    let storage = open_storage(db)?;
    let policy = Policy::from_env();
    let findings = collect_findings(&storage, &policy)?;
    let mut criticals = 0usize;
    for f in &findings {
        if f.severity == Severity::Critical {
            criticals += 1;
        }
        println!(
            "  [{}] {}  {}  → {}  — {}",
            f.severity.as_str(),
            f.rule,
            f.path,
            f.model,
            f.detail
        );
        storage.append_audit(
            "dlp",
            &format!(
                "severity={} rule={} task={} model={} path={}",
                f.severity.as_str(),
                f.rule,
                f.task,
                f.model,
                f.path
            ),
            chrono::Utc::now(),
        )?;
    }

    println!(
        "egress guard: {} violation(s), {criticals} critical",
        findings.len()
    );
    if strict && criticals > 0 {
        anyhow::bail!("{criticals} critical egress violation(s) — failing (--strict)");
    }
    Ok(())
}

/// Numeric rank for severity ordering (Info < Warn < Critical).
fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Info => 0,
        Severity::Warn => 1,
        Severity::Critical => 2,
    }
}

fn parse_severity(s: &str) -> Result<Severity> {
    match s.to_ascii_lowercase().as_str() {
        "info" => Ok(Severity::Info),
        "warn" | "warning" => Ok(Severity::Warn),
        "critical" | "crit" => Ok(Severity::Critical),
        other => anyhow::bail!("unknown severity '{other}' (use info|warn|critical)"),
    }
}

/// Where live DLP alerts go (§1.4). Slack takes precedence over a generic webhook; default stderr.
enum AlertSink {
    Stderr,
    Webhook(String),
    Slack(String),
}

impl AlertSink {
    fn from_opts(webhook: Option<String>, slack: Option<String>) -> Self {
        match (slack, webhook) {
            (Some(url), _) => Self::Slack(url),
            (None, Some(url)) => Self::Webhook(url),
            (None, None) => Self::Stderr,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Stderr => "stderr".to_owned(),
            Self::Webhook(u) => format!("webhook {u}"),
            Self::Slack(u) => format!("slack {u}"),
        }
    }

    fn emit(&self, f: &GuardFinding) -> Result<()> {
        let line = format!(
            "[{}] {} {} → {} — {}",
            f.severity.as_str(),
            f.rule,
            f.path,
            f.model,
            f.detail
        );
        match self {
            Self::Stderr => eprintln!("ALERT {line}"),
            Self::Webhook(url) => {
                let body = serde_json::json!({
                    "severity": f.severity.as_str(),
                    "rule": f.rule,
                    "path": f.path,
                    "model": f.model,
                    "detail": f.detail,
                    "task": f.task,
                });
                ureq::post(url)
                    .send_json(body)
                    .map_err(|e| anyhow::anyhow!("webhook POST failed: {e}"))?;
            }
            Self::Slack(url) => {
                let body = serde_json::json!({ "text": format!("brain0 DLP {line}") });
                ureq::post(url)
                    .send_json(body)
                    .map_err(|e| anyhow::anyhow!("slack POST failed: {e}"))?;
            }
        }
        Ok(())
    }
}

/// Live DLP daemon (§1.3): poll the index (kept fresh by `brain0 dev`/`observe`/MCP ingest), and
/// stream NEW policy violations at or above `min_severity` to the alert sink — deduped across ticks
/// by (task, path, rule, model). The first tick reports the current state, then only new findings.
fn cmd_guard_watch(
    db: &Path,
    interval_secs: u64,
    webhook: Option<String>,
    slack: Option<String>,
    min_severity: &str,
    once: bool,
) -> Result<()> {
    use std::collections::HashSet;
    let storage = open_storage(db)?;
    let policy = Policy::from_env();
    let floor = parse_severity(min_severity)?;
    let sink = AlertSink::from_opts(webhook, slack);
    let mut seen: HashSet<String> = HashSet::new();

    eprintln!(
        "brain0 watch: polling {} every {interval_secs}s, sink={}, min-severity={}",
        db.display(),
        sink.describe(),
        floor.as_str()
    );
    loop {
        let findings = collect_findings(&storage, &policy)?;
        let mut new_count = 0usize;
        for f in &findings {
            if severity_rank(f.severity) < severity_rank(floor) {
                continue;
            }
            let key = format!("{}|{}|{}|{}", f.task, f.path, f.rule, f.model);
            if seen.insert(key) {
                sink.emit(f)?;
                storage.append_audit(
                    "dlp-alert",
                    &format!(
                        "severity={} rule={} task={} model={} path={}",
                        f.severity.as_str(),
                        f.rule,
                        f.task,
                        f.model,
                        f.path
                    ),
                    chrono::Utc::now(),
                )?;
                new_count += 1;
            }
        }
        if new_count > 0 {
            eprintln!("brain0 watch: {new_count} new alert(s)");
        }
        if once {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(interval_secs.max(1)));
    }
    Ok(())
}

/// Preflight gate (§1.5): scan files an agent is ABOUT to read (or that are about to be committed)
/// for secrets + sensitive paths BEFORE they reach a remote model. Exits non-zero on a critical so
/// it can be wired as a git pre-commit hook or an agent/CI pre-run check. Pure prevention — no index.
fn cmd_preflight(files: &[String], staged: bool, warn_only: bool) -> Result<()> {
    let mut paths: Vec<String> = files.to_vec();
    if staged {
        let out = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only", "--diff-filter=ACM"])
            .output()
            .map_err(|e| anyhow::anyhow!("running git: {e}"))?;
        if !out.status.success() {
            anyhow::bail!("`git diff --cached` failed — not a git repository?");
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim();
            if !line.is_empty() {
                paths.push(line.to_owned());
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        anyhow::bail!("no files to check — pass paths or --staged");
    }

    let redactor = Redactor::new(&RedactionConfig::from_env())?;
    let policy = Policy::from_env();
    let mut checked = 0usize;
    let mut criticals = 0usize;
    let mut warns = 0usize;
    for path in &paths {
        let p = Path::new(path);
        if !p.is_file() {
            continue; // deleted/renamed-away or a directory
        }
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let content = String::from_utf8_lossy(&bytes);
        let kinds = redactor.scan_kinds(&content);
        checked += 1;
        // Preflight assumes the content WOULD reach a remote model — that's the scenario it prevents.
        let event = ReadEvent {
            path: path.clone(),
            external: is_external_path(path),
            secret_kinds: kinds,
            remote: true,
        };
        for v in evaluate(&policy, &event) {
            match v.severity {
                Severity::Critical => criticals += 1,
                Severity::Warn => warns += 1,
                Severity::Info => {}
            }
            println!(
                "  [{}] {}  {}  — {}",
                v.severity.as_str(),
                v.rule,
                v.path,
                v.detail
            );
        }
    }

    println!("preflight: {checked} file(s) checked, {criticals} critical, {warns} warning(s)");
    if criticals > 0 && !warn_only {
        anyhow::bail!(
            "preflight blocked: {criticals} critical violation(s) — sensitive content must not reach a remote model"
        );
    }
    Ok(())
}

/// Find the commit task (observer-originated: no source_adapter) whose git SHA (session id)
/// starts with `sha`.
fn find_commit_task(storage: &SqliteStorage, sha: &str) -> Result<Option<brain0_model::TaskId>> {
    for id in storage.all_task_ids()? {
        if let Some(node) = storage.get_task_node(&id)? {
            if node.source_adapter.is_none() && node.session_id.as_str().starts_with(sha) {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

/// Assemble + print the AI provenance of a commit: the agent sessions behind it (shared-version
/// join), each with model, reads, secrets read, and drift — the audit/attestation substrate (§2.1).
fn build_provenance(storage: &SqliteStorage, sha: &str) -> Result<serde_json::Value> {
    let commit_id = find_commit_task(storage, sha)?
        .ok_or_else(|| anyhow::anyhow!("no commit found for SHA prefix '{sha}'"))?;
    build_provenance_for(storage, &commit_id)
}

/// Assemble the provenance JSON for an already-resolved commit task (the shared-version join).
fn build_provenance_for(
    storage: &SqliteStorage,
    commit_id: &brain0_model::TaskId,
) -> Result<serde_json::Value> {
    use brain0_model::{Edge, EdgeKind};
    use std::collections::{BTreeMap, BTreeSet};

    let commit = storage
        .get_task_node(commit_id)?
        .ok_or_else(|| anyhow::anyhow!("commit task vanished"))?;

    // Files changed + the specific versions this commit produced (for the shared-version join).
    let mut changed_files = BTreeSet::new();
    let mut touched_versions = BTreeSet::new();
    let mut touched_artifacts = BTreeSet::new();
    for edge in storage.out_edges(EdgeKind::TaskModifiesArtifact, commit_id.as_str())? {
        if let Edge::TaskModifiesArtifact {
            artifact, version, ..
        } = edge
        {
            touched_versions.insert(version.as_str().to_owned());
            touched_artifacts.insert(artifact.as_str().to_owned());
            if let Some(a) = storage.get_artifact_node(&artifact)? {
                if a.level == Level::File {
                    changed_files.insert(a.qualified_path);
                }
            }
        }
    }

    // Agent sessions behind the commit: those linked to a version THIS commit produced.
    let mut agent_ids: BTreeSet<String> = BTreeSet::new();
    for art in &touched_artifacts {
        for edge in storage.in_edges(EdgeKind::TaskModifiesArtifact, art)? {
            if let Edge::TaskModifiesArtifact { task, version, .. } = edge {
                if task.as_str() != commit_id.as_str()
                    && touched_versions.contains(version.as_str())
                    && storage
                        .get_task_node(&task)?
                        .and_then(|n| n.source_adapter)
                        .is_some()
                {
                    agent_ids.insert(task.as_str().to_owned());
                }
            }
        }
    }

    let mut agents = Vec::new();
    for aid in &agent_ids {
        let tid = brain0_model::TaskId::new(aid.clone());
        let node = storage.get_task_node(&tid)?;
        let mut reads = BTreeSet::new();
        let mut secrets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut undeclared = BTreeSet::new();
        let mut phantom = BTreeSet::new();
        for v in storage.task_versions(&tid)? {
            reads.extend(v.reads);
            for rs in v.read_secrets {
                secrets.entry(rs.path).or_default().extend(rs.kinds);
            }
            if let Some(d) = v.drift {
                undeclared.extend(d.undeclared);
                phantom.extend(d.phantom);
            }
        }
        agents.push(serde_json::json!({
            "task": aid,
            "agent": node.as_ref().map(|n| n.agent.name.clone()),
            "model": node.as_ref().and_then(|n| n.model.clone()),
            "reads": reads.into_iter().collect::<Vec<_>>(),
            "read_secrets": secrets
                .into_iter()
                .map(|(p, k)| serde_json::json!({ "path": p, "kinds": k.into_iter().collect::<Vec<_>>() }))
                .collect::<Vec<_>>(),
            "drift_undeclared": undeclared.into_iter().collect::<Vec<_>>(),
            "drift_phantom": phantom.into_iter().collect::<Vec<_>>(),
        }));
    }

    let out = serde_json::json!({
        "commit": commit.session_id.as_str(),
        "task": commit_id.as_str(),
        "author": commit.author.name,
        "timestamp": commit.created_at.to_rfc3339(),
        "reviewed": !commit.reviewers.is_empty(),
        "reviewers": commit.reviewers,
        "changed_files": changed_files.into_iter().collect::<Vec<_>>(),
        "agents": agents,
    });
    Ok(out)
}

fn cmd_provenance(db: &Path, sha: &str) -> Result<()> {
    let storage = open_storage(db)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&build_provenance(&storage, sha)?)?
    );
    Ok(())
}

/// Load the attestation signing key (`BRAIN0_ATTEST_KEY` hex env, else `key_file`, generating a
/// fresh 0600 key file if absent). The signing key never appears in logs.
fn load_or_create_signer(key_file: &Path) -> Result<brain0_attest::Signer> {
    use brain0_attest::Signer;
    if let Ok(seed) = std::env::var("BRAIN0_ATTEST_KEY") {
        return Signer::from_seed_hex(&seed).map_err(|e| anyhow::anyhow!("BRAIN0_ATTEST_KEY: {e}"));
    }
    if key_file.exists() {
        let seed = std::fs::read_to_string(key_file)?;
        return Signer::from_seed_hex(seed.trim())
            .map_err(|e| anyhow::anyhow!("{key_file:?}: {e}"));
    }
    let (signer, seed) = Signer::generate().map_err(|e| anyhow::anyhow!(e))?;
    if let Some(parent) = key_file.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(key_file, &seed)?;
    let _ = brain0_crypto::restrict_permissions(key_file);
    eprintln!(
        "generated a new attestation signing key at {} (public key {})",
        key_file.display(),
        signer.public_key_hex()
    );
    Ok(signer)
}

/// Emit a signed AI-provenance attestation for a commit: an in-toto Statement (subject = the
/// commit, predicate = the provenance) wrapped in an Ed25519-signed envelope (`§2.4`).
fn cmd_attest(db: &Path, sha: &str, key_file: &Path) -> Result<()> {
    let storage = open_storage(db)?;
    let provenance = build_provenance(&storage, sha)?;
    let commit = provenance
        .get("commit")
        .and_then(|v| v.as_str())
        .unwrap_or(sha)
        .to_owned();
    let statement = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": format!("git+commit:{commit}"), "digest": { "sha1": commit } }],
        "predicateType": "https://brain0.dev/ai-provenance/v0.1",
        "predicate": provenance,
    });
    // Sign the exact serialized statement bytes (the verifier checks the same bytes back).
    let statement_str = serde_json::to_string(&statement)?;
    let signer = load_or_create_signer(key_file)?;
    let signature = signer.sign(statement_str.as_bytes());
    let envelope = serde_json::json!({
        "statement": statement_str,
        "keyid": signer.key_id(),
        "publicKey": signer.public_key_hex(),
        "signature": signature,
    });
    println!("{}", serde_json::to_string_pretty(&envelope)?);
    Ok(())
}

/// Verify a signed attestation envelope. Use `--pubkey <trusted>` to check against a known key;
/// otherwise the embedded public key is used (convenient, but only proves internal consistency).
fn cmd_verify_attestation(file: &Path, pubkey: Option<&str>) -> Result<()> {
    let raw = std::fs::read_to_string(file)?;
    let envelope: serde_json::Value = serde_json::from_str(&raw)?;
    let field = |k: &str| envelope.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let statement =
        field("statement").ok_or_else(|| anyhow::anyhow!("envelope missing 'statement'"))?;
    let signature =
        field("signature").ok_or_else(|| anyhow::anyhow!("envelope missing 'signature'"))?;
    let pk = pubkey
        .map(str::to_owned)
        .or_else(|| field("publicKey"))
        .ok_or_else(|| {
            anyhow::anyhow!("no public key: pass --pubkey <hex> or include publicKey")
        })?;
    match brain0_attest::verify(statement.as_bytes(), &signature, &pk) {
        Ok(()) => {
            println!("attestation OK (key {})", brain0_attest::key_id_for(&pk));
            if pubkey.is_none() {
                eprintln!("note: verified against the EMBEDDED public key; pass --pubkey <trusted> to check a known key");
            }
            Ok(())
        }
        Err(e) => anyhow::bail!("attestation INVALID: {e}"),
    }
}

/// Parse the new-side hunk ranges from a unified diff (`@@ -a,b +c,d @@` → lines `c..c+d-1`).
fn parse_hunks(diff: &str) -> Vec<serde_json::Value> {
    let mut hunks = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("@@") else {
            continue;
        };
        // The new-side spec is the whitespace token starting with '+'.
        let Some(plus) = rest.split_whitespace().find(|t| t.starts_with('+')) else {
            continue;
        };
        let mut parts = plus[1..].split(',');
        let start: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let count: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        if count == 0 {
            continue; // pure deletion — no new-side lines
        }
        hunks.push(serde_json::json!({
            "start": start,
            "lines": count,
            "end": start + count - 1,
        }));
    }
    hunks
}

/// Per-file / per-hunk attribution for a commit (§2.2): the changed line ranges (from the stored
/// unified diff) plus the model(s) behind the commit and its review status.
fn cmd_attribution(
    db: &Path,
    payload: &Path,
    key_file: &Path,
    encrypt: bool,
    sha: &str,
) -> Result<()> {
    use brain0_model::{Edge, EdgeKind};
    use std::collections::BTreeSet;

    let storage = open_storage(db)?;
    let store = open_payload(payload, encrypt, key_file)?;
    let commit_id = find_commit_task(&storage, sha)?
        .ok_or_else(|| anyhow::anyhow!("no commit found for SHA prefix '{sha}'"))?;
    let prov = build_provenance_for(&storage, &commit_id)?;

    let mut models: BTreeSet<String> = BTreeSet::new();
    if let Some(agents) = prov.get("agents").and_then(|v| v.as_array()) {
        for a in agents {
            if let Some(m) = a.get("model").and_then(|v| v.as_str()) {
                models.insert(m.to_owned());
            }
        }
    }

    let mut files = Vec::new();
    for edge in storage.out_edges(EdgeKind::TaskModifiesArtifact, commit_id.as_str())? {
        if let Edge::TaskModifiesArtifact {
            artifact,
            version,
            lines_added,
            lines_removed,
            ..
        } = edge
        {
            let Some(node) = storage.get_artifact_node(&artifact)? else {
                continue;
            };
            if node.level != Level::File {
                continue;
            }
            let diff_ref = storage
                .artifact_versions(&artifact)?
                .into_iter()
                .find(|v| v.id.as_str() == version.as_str())
                .and_then(|v| v.diff_ref);
            let hunks = match diff_ref {
                Some(r) => store
                    .get_str(&r)?
                    .map(|d| parse_hunks(&d))
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            files.push(serde_json::json!({
                "file": node.qualified_path,
                "lines_added": lines_added,
                "lines_removed": lines_removed,
                "hunks": hunks,
            }));
        }
    }

    let out = serde_json::json!({
        "commit": prov.get("commit"),
        "reviewed": prov.get("reviewed"),
        "models": models.into_iter().collect::<Vec<_>>(),
        "files": files,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Compliance pack (§2.5): aggregate AI-provenance across every indexed commit into an
/// auditor-facing report — how many commits were AI-assisted, which models, and where a
/// secret-bearing file reached a remote model. `--json` for machine consumption; `--strict` exits
/// non-zero if any secret-bearing read is found (CI gate for a release/PR).
fn cmd_compliance(db: &Path, json: bool, strict: bool) -> Result<()> {
    use std::collections::BTreeSet;
    let storage = open_storage(db)?;

    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut total_commits = 0usize;
    let mut ai_assisted = 0usize;
    let mut commits_with_secret_reads = 0usize;
    let mut total_secret_reads = 0usize;
    let mut total_drift = 0usize;
    let mut ai_unreviewed = 0usize;

    for id in storage.all_task_ids()? {
        let Some(node) = storage.get_task_node(&id)? else {
            continue;
        };
        if node.source_adapter.is_some() {
            continue; // agent sessions; commits are observer-originated
        }
        total_commits += 1;
        let reviewed = !node.reviewers.is_empty();
        let prov = build_provenance_for(&storage, &id)?;
        let agents = prov
            .get("agents")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let ai_assisted_commit = !agents.is_empty();
        if ai_assisted_commit {
            ai_assisted += 1;
            if !reviewed {
                ai_unreviewed += 1;
            }
        }
        let mut commit_models: BTreeSet<String> = BTreeSet::new();
        let mut secret_reads = 0usize;
        let mut drift = 0usize;
        for a in &agents {
            if let Some(m) = a.get("model").and_then(|v| v.as_str()) {
                commit_models.insert(m.to_owned());
                models.insert(m.to_owned());
            }
            secret_reads += a
                .get("read_secrets")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            drift += a
                .get("drift_undeclared")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
        }
        if secret_reads > 0 {
            commits_with_secret_reads += 1;
        }
        total_secret_reads += secret_reads;
        total_drift += drift;
        rows.push(serde_json::json!({
            "commit": prov.get("commit"),
            "author": prov.get("author"),
            "timestamp": prov.get("timestamp"),
            "ai_assisted": ai_assisted_commit,
            "reviewed": reviewed,
            "agents": agents.len(),
            "models": commit_models.into_iter().collect::<Vec<_>>(),
            "secret_reads": secret_reads,
            "drift_undeclared": drift,
            "changed_files": prov.get("changed_files").and_then(|v| v.as_array()).map_or(0, Vec::len),
        }));
    }

    if json {
        let report = serde_json::json!({
            "summary": {
                "commits": total_commits,
                "ai_assisted_commits": ai_assisted,
                "ai_assisted_unreviewed": ai_unreviewed,
                "models": models.iter().collect::<Vec<_>>(),
                "commits_with_secret_reads": commits_with_secret_reads,
                "total_secret_reads": total_secret_reads,
                "total_undeclared_drift": total_drift,
            },
            "commits": rows,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let models_str = if models.is_empty() {
            "(none recorded)".to_owned()
        } else {
            models.iter().cloned().collect::<Vec<_>>().join(", ")
        };
        println!("brain0 compliance report");
        println!("  commits indexed:          {total_commits}");
        println!("  AI-assisted commits:      {ai_assisted}");
        println!("  └─ unreviewed (no human): {ai_unreviewed}");
        println!("  models seen:              {models_str}");
        println!("  commits w/ secret reads:  {commits_with_secret_reads}  (total {total_secret_reads} secret-file read(s))");
        println!("  undeclared drift events:  {total_drift}");
        let secret_commits: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|r| r["secret_reads"].as_u64().unwrap_or(0) > 0)
            .collect();
        if !secret_commits.is_empty() {
            println!("\n  \u{26a0} commits where a secret-bearing file reached a remote model:");
            for r in secret_commits {
                println!(
                    "    {} — {} secret read(s), models {}",
                    r["commit"].as_str().unwrap_or("?"),
                    r["secret_reads"],
                    r["models"]
                );
            }
        }
        let unreviewed: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|r| {
                r["ai_assisted"].as_bool() == Some(true) && r["reviewed"].as_bool() == Some(false)
            })
            .collect();
        if !unreviewed.is_empty() {
            println!("\n  \u{26a0} AI-assisted commits with no human review (no Reviewed-by/Acked-by trailer):");
            for r in unreviewed {
                println!(
                    "    {} — models {}",
                    r["commit"].as_str().unwrap_or("?"),
                    r["models"]
                );
            }
        }
    }

    if strict && total_secret_reads > 0 {
        anyhow::bail!(
            "{total_secret_reads} secret-bearing read(s) across {commits_with_secret_reads} commit(s)"
        );
    }
    Ok(())
}

fn cmd_audit(db: &Path, limit: usize) -> Result<()> {
    let storage = open_storage(db)?;
    for event in storage.list_audit(limit)? {
        println!(
            "{}  {}  {}",
            event.timestamp.to_rfc3339(),
            event.event_type,
            event.detail
        );
    }
    Ok(())
}

/// `brain0 rewind`: list checkpoints, or restore one — recording a pre-rewind checkpoint of
/// the CURRENT state first, so the restore is itself reversible.
#[allow(clippy::too_many_arguments)]
fn cmd_rewind(
    to: Option<&str>,
    list: bool,
    path: &Path,
    db: &Path,
    payload: &Path,
    key_file: &Path,
    encrypt: bool,
) -> Result<()> {
    let storage = open_storage(db)?;
    let payload_store = open_payload(payload, encrypt, key_file)?;

    if list || to.is_none() {
        let checkpoints = storage.list_checkpoints()?;
        if checkpoints.is_empty() {
            println!("no checkpoints recorded — run `brain0 watch` to build the safety net");
        }
        for (id, at, files) in checkpoints {
            println!("  {id}  {at}  ({files} files)");
        }
        return Ok(());
    }
    let target = to.expect("checked above");

    // Safety first: record the CURRENT tree as a checkpoint, so this rewind can be undone.
    let now = chrono::Utc::now();
    let tree = brain0_observer::full_tree(path)?;
    let pre = brain0_observer::snapshot_directory(
        path,
        "rewind",
        brain0_model::Author::new("rewind"),
        brain0_model::Agent::human(),
        now,
    )?;
    let pre_id = pre.source.ref_str().to_owned();
    rewind::persist_manifest(&storage, payload_store.as_ref(), &pre_id, &now, &tree)?;
    storage.append_audit("rewind", &format!("pre-rewind checkpoint {pre_id}"), now)?;

    let written = rewind::restore(&storage, payload_store.as_ref(), target, path)?;
    storage.append_audit(
        "rewind",
        &format!("restored {written} file(s) from {target}"),
        chrono::Utc::now(),
    )?;
    println!("restored {written} file(s) from {target}");
    println!("  undo with: brain0 rewind --to {pre_id}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_watch(
    repo: &str,
    path: &Path,
    db: &Path,
    payload: &Path,
    author: &str,
    debounce_ms: u64,
    key_file: &Path,
    encrypt: bool,
) -> Result<()> {
    let storage = open_storage(db)?;
    let payload_store = open_payload(payload, encrypt, key_file)?;
    let mut engine = CheckpointEngine::new(
        path,
        repo,
        brain0_model::Author::new(author),
        brain0_model::Agent::human(),
    );
    println!("watching {} (Ctrl-C to stop)…", path.display());
    let watch_root = path.to_path_buf();
    engine.watch(Duration::from_millis(debounce_ms), |snapshot| {
        if let Err(err) = ingest_snapshot(&storage, payload_store.as_ref(), snapshot) {
            eprintln!("ingest error: {err}");
        }
        // Safety net: persist the FULL tree per checkpoint (content-addressed → deduped),
        // so `brain0 rewind --to <id>` can restore this exact state later.
        match brain0_observer::full_tree(&watch_root) {
            Ok(tree) => {
                if let Err(err) = rewind::persist_manifest(
                    &storage,
                    payload_store.as_ref(),
                    snapshot.source.ref_str(),
                    &snapshot.timestamp,
                    &tree,
                ) {
                    eprintln!("manifest error: {err}");
                }
            }
            Err(err) => eprintln!("manifest scan error: {err}"),
        }
        if let Err(err) = recompute_risk(&storage, repo) {
            eprintln!("risk error: {err}");
        }
    })?;
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Ingest {
            repo,
            path,
            db,
            payload,
            author,
            key_file,
            no_encrypt_payload,
        } => cmd_ingest(
            &repo,
            &path,
            &db,
            &payload,
            &author,
            &key_file,
            !no_encrypt_payload,
        ),
        Command::Observe {
            repo,
            path,
            db,
            payload,
            all,
            key_file,
            no_encrypt_payload,
        } => cmd_observe(
            repo.as_deref(),
            &path,
            &db,
            &payload,
            all,
            &key_file,
            !no_encrypt_payload,
        ),
        Command::Query { text, db, k } => cmd_query(&text, &db, k),
        Command::Report { repo, db, md, top } => cmd_report(repo.as_deref(), &db, md, top),
        Command::Today { db, since } => cmd_today(&db, &since),
        Command::Rewind {
            to,
            list,
            path,
            db,
            payload,
            key_file,
            no_encrypt_payload,
        } => cmd_rewind(
            to.as_deref(),
            list,
            &path,
            &db,
            &payload,
            &key_file,
            !no_encrypt_payload,
        ),
        Command::Mcp { db, repo } => cmd_mcp(&db, repo),
        Command::Verify {
            db,
            payload,
            key_file,
            no_encrypt_payload,
        } => cmd_verify(&db, &payload, &key_file, !no_encrypt_payload),
        Command::Audit { db, limit } => cmd_audit(&db, limit),
        Command::Guard {
            db,
            strict,
            watch,
            interval,
            webhook,
            slack,
            min_severity,
            once,
        } => {
            if watch {
                cmd_guard_watch(&db, interval, webhook, slack, &min_severity, once)
            } else {
                cmd_guard(&db, strict)
            }
        }
        Command::Preflight {
            files,
            staged,
            warn_only,
        } => cmd_preflight(&files, staged, warn_only),
        Command::Provenance { commit, db } => cmd_provenance(&db, &commit),
        Command::Attest {
            commit,
            db,
            key_file,
        } => cmd_attest(&db, &commit, &key_file),
        Command::VerifyAttestation { file, pubkey } => {
            cmd_verify_attestation(&file, pubkey.as_deref())
        }
        Command::Attribution {
            commit,
            db,
            payload,
            key_file,
            no_encrypt_payload,
        } => cmd_attribution(&db, &payload, &key_file, !no_encrypt_payload, &commit),
        Command::Compliance { db, json, strict } => cmd_compliance(&db, json, strict),
        Command::Reembed {
            db,
            payload,
            key_file,
            no_encrypt_payload,
        } => cmd_reembed(&db, &payload, &key_file, !no_encrypt_payload),
        Command::Purge {
            db,
            payload,
            key_file,
            no_encrypt_payload,
            task,
            older_than_days,
        } => cmd_purge(
            &db,
            &payload,
            &key_file,
            !no_encrypt_payload,
            task.as_deref(),
            older_than_days,
        ),
        Command::Watch {
            repo,
            path,
            db,
            payload,
            author,
            debounce_ms,
            key_file,
            no_encrypt_payload,
        } => cmd_watch(
            &repo,
            &path,
            &db,
            &payload,
            &author,
            debounce_ms,
            &key_file,
            !no_encrypt_payload,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_hunks;

    #[test]
    fn parse_hunks_reads_new_side_ranges() {
        let diff = "\
--- a/f.py
+++ b/f.py
@@ -1,3 +1,4 @@ def f():
 a
+b
 c
@@ -10,2 +11,0 @@
-gone
@@ -20,0 +22,3 @@
+x
+y
+z
";
        let hunks = parse_hunks(diff);
        // The pure-deletion hunk (+11,0) is skipped; two new-side hunks remain.
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0]["start"], 1);
        assert_eq!(hunks[0]["lines"], 4);
        assert_eq!(hunks[0]["end"], 4);
        assert_eq!(hunks[1]["start"], 22);
        assert_eq!(hunks[1]["end"], 24);
    }
}
