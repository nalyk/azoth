//! `azoth eval run --live-retrieval <repo>` — replaces each seed
//! task's `predicted_files` with the top-k paths surfaced by a real
//! composite retrieval pass against `<repo>`, so localization@k
//! measures retrieval quality rather than seed-authoring quality.
//!
//! Default behaviour (no flag) is still seed-vs-seed — CI
//! reproducibility matters more than the absolute metric on that
//! path. Live-retrieval flips the interpretation: the seed file
//! becomes a ground-truth probe, not a frozen answer key. The
//! emitted `EvalSampled.metric` is `localization_precision_at_k_live`
//! so forensic consumers can split the two modes in the SQLite
//! mirror's `eval_runs` table.
//!
//! ## Composite shape
//!
//! Three of the four composite lanes are wired here — FTS, symbols,
//! ripgrep — with `ReciprocalRankFusion` as the reranker. The graph
//! lane is intentionally left unwired at this layer: PR B wires it
//! into the runtime composite once `GraphEvidenceCollector` lands,
//! and this module picks it up through the same builder. Keeping the
//! two PRs decoupled means live retrieval ships without waiting on
//! the graph-extractor design question.
//!
//! ## Path extraction
//!
//! `EvidenceItem.label` and `artifact_ref` carry path data in two
//! shapes today:
//!
//! - lexical/FTS lanes: `label = "{path}:{start_line}"`
//! - symbol lane: `label = "symbol {name} ({kind})"`,
//!   `artifact_ref = "{path}#L{line}"`
//!
//! The graph lane (PR B) will write `label = "{path}"` directly.
//! `extract_path` handles all three shapes and falls back to the raw
//! label when neither separator is present.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use azoth_core::context::{
    CompositeEvidenceCollector, EvidenceCollector, LexicalEvidenceCollector, ReciprocalRankFusion,
    SymbolEvidenceCollector, TokenBudget,
};
use azoth_core::eval::SeedTask;
use azoth_core::retrieval::{
    CoEditConfig, LexicalRetrieval, RipgrepLexicalRetrieval, SymbolRetrieval,
};
use azoth_core::schemas::EvidenceItem;
use azoth_repo::history::co_edit;
use azoth_repo::{FtsLexicalRetrieval, RepoIndexer, SqliteSymbolIndex};

/// Per-sweep stats so the caller can report "retrieval produced
/// anything at all" vs "every task came back empty" — the latter is
/// a silent-failure smell worth catching.
#[derive(Debug, Clone, Default)]
pub struct LiveRetrievalStats {
    pub tasks_processed: u32,
    pub total_predictions: u32,
    pub tasks_with_zero_predictions: u32,
}

#[derive(Debug)]
pub enum LiveRetrievalError {
    Io(std::io::Error),
    Indexer(String),
    Retrieval(String),
}

impl std::fmt::Display for LiveRetrievalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveRetrievalError::Io(e) => write!(f, "io: {e}"),
            LiveRetrievalError::Indexer(s) => write!(f, "indexer: {s}"),
            LiveRetrievalError::Retrieval(s) => write!(f, "retrieval: {s}"),
        }
    }
}

impl std::error::Error for LiveRetrievalError {}

impl From<std::io::Error> for LiveRetrievalError {
    fn from(e: std::io::Error) -> Self {
        LiveRetrievalError::Io(e)
    }
}

/// Pulls a path out of a single `EvidenceItem`, handling every shape
/// the v2 collectors emit. Order matters: symbol lane uses the label
/// as a human-readable tag and stashes the real path in
/// `artifact_ref`, so we consult it first when the label looks like
/// a symbol tag.
pub fn extract_path(item: &EvidenceItem) -> Option<String> {
    if item.label.starts_with("symbol ") {
        if let Some(a) = &item.artifact_ref {
            if let Some((path, _line)) = a.split_once("#L") {
                return Some(normalise_path(path));
            }
            return Some(normalise_path(a));
        }
        return None;
    }
    if let Some((path, _rest)) = item.label.split_once(':') {
        return Some(normalise_path(path));
    }
    Some(normalise_path(&item.label))
}

fn normalise_path(p: &str) -> String {
    let trimmed = p.trim();
    let no_prefix = trimmed.strip_prefix("./").unwrap_or(trimmed);
    no_prefix.replace('\\', "/")
}

/// Run the composite collector against `repo_root` for each task
/// and overwrite `predicted_files` with the first k distinct paths.
/// Returns stats for logging.
pub async fn apply_live_retrieval(
    repo_root: &Path,
    tasks: &mut [SeedTask],
    k: u32,
) -> Result<LiveRetrievalStats, LiveRetrievalError> {
    let collector = build_collector(repo_root, k).await?;
    apply_with_collector(collector.as_ref(), tasks, k).await
}

/// Plumbing split out so tests can pass a `StaticCollector` / any
/// `EvidenceCollector` without standing up a full index.
pub async fn apply_with_collector(
    collector: &dyn EvidenceCollector,
    tasks: &mut [SeedTask],
    k: u32,
) -> Result<LiveRetrievalStats, LiveRetrievalError> {
    let mut stats = LiveRetrievalStats::default();
    let limit = (k as usize).max(1);
    for task in tasks.iter_mut() {
        stats.tasks_processed += 1;
        let items = collector
            .collect(&task.prompt, limit * 2)
            .await
            .map_err(|e| LiveRetrievalError::Retrieval(e.to_string()))?;
        let predicted = dedup_preserving_order(items.iter().filter_map(extract_path), limit);
        if predicted.is_empty() {
            stats.tasks_with_zero_predictions += 1;
        }
        stats.total_predictions += predicted.len() as u32;
        task.predicted_files = predicted;
    }
    Ok(stats)
}

fn dedup_preserving_order<I: IntoIterator<Item = String>>(iter: I, cap: usize) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::with_capacity(cap);
    for p in iter {
        if out.len() >= cap {
            break;
        }
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out
}

async fn build_collector(
    repo_root: &Path,
    _k: u32,
) -> Result<Arc<dyn EvidenceCollector>, LiveRetrievalError> {
    let repo_root = repo_root.canonicalize()?;
    let azoth_dir = repo_root.join(".azoth");
    std::fs::create_dir_all(&azoth_dir)?;
    let db_path = azoth_dir.join("state.sqlite");

    let indexer = RepoIndexer::open(&db_path, repo_root.clone())
        .map_err(|e| LiveRetrievalError::Indexer(e.to_string()))?;
    let _ = indexer
        .reindex_incremental()
        .await
        .map_err(|e| LiveRetrievalError::Indexer(e.to_string()))?;

    let co_edit_conn = indexer.connection();
    let co_edit_root = repo_root.clone();
    let _ = tokio::task::spawn_blocking(move || {
        co_edit::build(&co_edit_conn, &co_edit_root, CoEditConfig::default())
    })
    .await;
    drop(indexer);

    let fts = FtsLexicalRetrieval::open(&db_path)
        .map_err(|e| LiveRetrievalError::Indexer(e.to_string()))?;
    let fts_arc: Arc<FtsLexicalRetrieval> = Arc::new(fts);
    let symbols = SqliteSymbolIndex::open(&db_path)
        .map_err(|e| LiveRetrievalError::Indexer(e.to_string()))?;
    let symbols_arc: Arc<SqliteSymbolIndex> = Arc::new(symbols);

    let ripgrep_arc: Arc<dyn LexicalRetrieval> = Arc::new(RipgrepLexicalRetrieval {
        root: repo_root.clone(),
    });
    let fts_dyn: Arc<dyn LexicalRetrieval> = fts_arc.clone();
    let symbols_dyn: Arc<dyn SymbolRetrieval> = symbols_arc.clone();

    let ripgrep_lane: Arc<dyn EvidenceCollector> =
        Arc::new(LexicalEvidenceCollector::new(ripgrep_arc));
    let fts_lane: Arc<dyn EvidenceCollector> = Arc::new(LexicalEvidenceCollector::new(fts_dyn));
    let symbol_lane: Arc<dyn EvidenceCollector> =
        Arc::new(SymbolEvidenceCollector::new(symbols_dyn));

    let mut budget = TokenBudget::v2_default();
    budget.max_tokens = 8192;
    let composite = CompositeEvidenceCollector {
        graph: None,
        symbol: Some(symbol_lane),
        lexical: Some(ripgrep_lane),
        fts: Some(fts_lane),
        reranker: Arc::new(ReciprocalRankFusion::default()),
        budget,
        per_lane_limit: 20,
    };
    Ok(Arc::new(composite))
}

/// Resolve `--live-retrieval <repo>` in the CLI. Kept here so
/// `eval.rs` stays pure-serde and doesn't pull `tokio::runtime`.
pub fn resolve_repo(flag: PathBuf) -> Result<PathBuf, LiveRetrievalError> {
    if !flag.exists() {
        return Err(LiveRetrievalError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("--live-retrieval path does not exist: {}", flag.display()),
        )));
    }
    Ok(flag)
}

/// Exposed so tests can assert the metric label the runner emits.
pub const METRIC_LIVE: &str = "localization_precision_at_k_live";

/// `eval.rs` stitches the stats into the human-readable report.
pub fn format_stats(stats: &LiveRetrievalStats) -> String {
    format!(
        "live retrieval: tasks={} predictions={} empty={}",
        stats.tasks_processed, stats.total_predictions, stats.tasks_with_zero_predictions
    )
}

/// Small convenience shared by `apply_live_retrieval` tests — lift
/// a `HashMap<task_id, predictions>` into `predicted_files` when you
/// already have the predictions from some other backend. Keeps the
/// test code from hand-mutating `SeedTask` vectors.
pub fn inject_predictions(tasks: &mut [SeedTask], predictions: &HashMap<String, Vec<String>>) {
    for t in tasks.iter_mut() {
        if let Some(p) = predictions.get(&t.id) {
            t.predicted_files = p.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use azoth_core::retrieval::RetrievalError;

    fn lex(path: &str, line: u32, w: u32) -> EvidenceItem {
        EvidenceItem {
            label: format!("{path}:{line}"),
            artifact_ref: None,
            inline: None,
            decision_weight: w,
            lane: None,
            rerank_score: None,
        }
    }

    fn sym(name: &str, path: &str, line: u32, w: u32) -> EvidenceItem {
        EvidenceItem {
            label: format!("symbol {name} (fn)"),
            artifact_ref: Some(format!("{path}#L{line}")),
            inline: None,
            decision_weight: w,
            lane: None,
            rerank_score: None,
        }
    }

    #[test]
    fn extract_path_from_lexical_item() {
        let got = extract_path(&lex("crates/azoth-core/src/turn/mod.rs", 42, 10)).unwrap();
        assert_eq!(got, "crates/azoth-core/src/turn/mod.rs");
    }

    #[test]
    fn extract_path_from_symbol_item_uses_artifact_ref() {
        let got =
            extract_path(&sym("foo", "crates/azoth-core/src/adapter/mod.rs", 120, 5)).unwrap();
        assert_eq!(got, "crates/azoth-core/src/adapter/mod.rs");
    }

    #[test]
    fn extract_path_graph_lane_fallback_keeps_bare_label() {
        let item = EvidenceItem {
            label: "crates/azoth-core/src/execution/dispatcher.rs".into(),
            artifact_ref: None,
            inline: None,
            decision_weight: 7,
            lane: Some("graph".into()),
            rerank_score: None,
        };
        let got = extract_path(&item).unwrap();
        assert_eq!(got, "crates/azoth-core/src/execution/dispatcher.rs");
    }

    #[test]
    fn extract_path_strips_dotslash_and_normalises_separators() {
        let item = lex("./src/foo.rs", 1, 5);
        assert_eq!(extract_path(&item).unwrap(), "src/foo.rs");

        let mut win = lex(r"src\foo.rs", 1, 5);
        win.label = r"src\foo.rs:1".into();
        assert_eq!(extract_path(&win).unwrap(), "src/foo.rs");
    }

    struct StaticCollector(Vec<EvidenceItem>);

    #[async_trait]
    impl EvidenceCollector for StaticCollector {
        async fn collect(
            &self,
            _q: &str,
            _limit: usize,
        ) -> Result<Vec<EvidenceItem>, RetrievalError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn apply_with_collector_overwrites_predicted_files_with_extracted_paths() {
        let tasks_items = vec![
            lex("crates/azoth-core/src/turn/mod.rs", 42, 20),
            sym(
                "turn_driver",
                "crates/azoth-core/src/turn/driver.rs",
                88,
                18,
            ),
            lex("README.md", 1, 1),
        ];
        let collector = StaticCollector(tasks_items);

        let mut tasks = vec![SeedTask {
            id: "t1".into(),
            prompt: "biased select cancellation".into(),
            relevant_files: vec!["crates/azoth-core/src/turn/driver.rs".into()],
            predicted_files: vec!["SEED_STALE.md".into()],
            notes: String::new(),
        }];

        let stats = apply_with_collector(&collector, &mut tasks, 5)
            .await
            .unwrap();
        assert_eq!(stats.tasks_processed, 1);
        assert_eq!(stats.tasks_with_zero_predictions, 0);
        assert_eq!(stats.total_predictions, 3);
        assert_eq!(
            tasks[0].predicted_files,
            vec![
                "crates/azoth-core/src/turn/mod.rs".to_string(),
                "crates/azoth-core/src/turn/driver.rs".to_string(),
                "README.md".to_string(),
            ],
            "predicted_files must reflect live retrieval output, not the seed"
        );
    }

    #[tokio::test]
    async fn apply_with_collector_caps_at_k_and_dedupes() {
        let mut items = Vec::new();
        for i in 0..10 {
            // Same path at different lines — should dedupe to one
            // entry since localization@k is file-level.
            items.push(lex("crates/azoth-core/src/turn/mod.rs", i, 20 - i));
        }
        for i in 0..5 {
            items.push(lex(&format!("crates/other/file_{i}.rs"), 1, 5));
        }
        let collector = StaticCollector(items);
        let mut tasks = vec![SeedTask {
            id: "t1".into(),
            prompt: "dedupe test".into(),
            relevant_files: vec!["crates/azoth-core/src/turn/mod.rs".into()],
            predicted_files: vec![],
            notes: String::new(),
        }];

        let stats = apply_with_collector(&collector, &mut tasks, 3)
            .await
            .unwrap();
        assert_eq!(stats.total_predictions, 3);
        assert_eq!(tasks[0].predicted_files.len(), 3);
        assert_eq!(
            tasks[0].predicted_files[0], "crates/azoth-core/src/turn/mod.rs",
            "first-seen path wins dedupe"
        );
        // Must not contain two "turn/mod.rs" entries.
        let mod_hits = tasks[0]
            .predicted_files
            .iter()
            .filter(|p| p.as_str() == "crates/azoth-core/src/turn/mod.rs")
            .count();
        assert_eq!(mod_hits, 1, "duplicate paths must collapse to one");
    }

    #[tokio::test]
    async fn apply_with_collector_records_empty_predictions_in_stats() {
        let collector = StaticCollector(vec![]);
        let mut tasks = vec![
            SeedTask {
                id: "t1".into(),
                prompt: "q1".into(),
                relevant_files: vec![],
                predicted_files: vec![],
                notes: String::new(),
            },
            SeedTask {
                id: "t2".into(),
                prompt: "q2".into(),
                relevant_files: vec![],
                predicted_files: vec![],
                notes: String::new(),
            },
        ];
        let stats = apply_with_collector(&collector, &mut tasks, 5)
            .await
            .unwrap();
        assert_eq!(stats.tasks_processed, 2);
        assert_eq!(stats.tasks_with_zero_predictions, 2);
        assert_eq!(stats.total_predictions, 0);
        assert!(tasks.iter().all(|t| t.predicted_files.is_empty()));
    }

    #[test]
    fn resolve_repo_errors_when_path_missing() {
        let bad = PathBuf::from("/definitely/not/a/real/path/for/live-retrieval-test");
        assert!(resolve_repo(bad).is_err());
    }

    /// End-to-end smoke: real `RepoIndexer` + FTS + composite over
    /// a throwaway tempdir repo. Proves the wiring between
    /// `apply_live_retrieval` and the three azoth-repo backends is
    /// intact, not just that the in-memory extractor works.
    ///
    /// Intentionally small (two files, no git history) so the test
    /// is fast and deterministic. Metric calibration against the
    /// real azoth repo is a CLI-level dogfood, not a unit concern.
    #[tokio::test]
    async fn live_retrieval_against_real_tempdir_repo_produces_nonzero_predictions() {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path();

        std::fs::write(
            root.join("alpha.rs"),
            "pub fn cancel_turn_driver() { /* biased select lives here */ }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("beta.rs"),
            "pub struct TurnDriver;\nimpl TurnDriver { pub fn new() -> Self { Self } }\n",
        )
        .unwrap();

        let mut tasks = vec![SeedTask {
            id: "smoke".into(),
            prompt: "cancel_turn_driver biased".into(),
            relevant_files: vec!["alpha.rs".into()],
            predicted_files: vec!["SEED_STALE_MUST_BE_OVERWRITTEN.md".into()],
            notes: "live retrieval smoke".into(),
        }];

        let stats = apply_live_retrieval(root, &mut tasks, 5)
            .await
            .expect("live retrieval must succeed against tempdir repo");

        assert_eq!(stats.tasks_processed, 1);
        assert!(
            !tasks[0]
                .predicted_files
                .contains(&"SEED_STALE_MUST_BE_OVERWRITTEN.md".to_string()),
            "live retrieval must overwrite predicted_files; got {:?}",
            tasks[0].predicted_files
        );
        assert!(
            !tasks[0].predicted_files.is_empty(),
            "live retrieval produced zero predictions despite matching terms in the repo"
        );
        assert!(
            tasks[0]
                .predicted_files
                .iter()
                .any(|p| p.ends_with("alpha.rs")),
            "alpha.rs (contains cancel_turn_driver) should appear in predictions; got {:?}",
            tasks[0].predicted_files
        );
    }

    #[test]
    fn inject_predictions_overrides_matching_ids_only() {
        let mut tasks = vec![
            SeedTask {
                id: "keep".into(),
                prompt: "q".into(),
                relevant_files: vec![],
                predicted_files: vec!["original".into()],
                notes: String::new(),
            },
            SeedTask {
                id: "replace".into(),
                prompt: "q".into(),
                relevant_files: vec![],
                predicted_files: vec!["original".into()],
                notes: String::new(),
            },
        ];
        let mut p: HashMap<String, Vec<String>> = HashMap::new();
        p.insert("replace".into(), vec!["live".into()]);
        inject_predictions(&mut tasks, &p);
        assert_eq!(tasks[0].predicted_files, vec!["original".to_string()]);
        assert_eq!(tasks[1].predicted_files, vec!["live".to_string()]);
    }
}
