//! JSONL session log — append-only writer, dual-projection reader.
//!
//! Two projections read the same file:
//!
//! * **Replayable** (what the Context Kernel trusts): emits only events from
//!   turns whose last marker is `turn_committed`. Dangling or interrupted or
//!   aborted turns are *dropped whole* — no orphaned `tool_result` blocks
//!   can leak into a rebuilt context.
//!
//! * **Forensic** (what `/status`, postmortems, and the eval plane read):
//!   emits every line, tagging events that belong to non-committed turns
//!   with `non_replayable: true`.
//!
//! On load, any turn without a terminal marker is *closed* by appending a
//! synthetic `turn_interrupted { reason: "crash" }` record.

use crate::event_store::sqlite::SqliteMirror;
use crate::schemas::{
    AbortReason, EffectClass, EffectCounter, Message, Role, SessionEvent, TurnId, Usage,
};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Error)]
pub enum ProjectionError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("parse at line {line}: {source}")]
    Parse {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("malformed as-of timestamp {input:?}: {detail}")]
    MalformedAsOf { input: String, detail: String },
}

/// Parse an RFC3339 timestamp into an `OffsetDateTime`. Used to compare
/// event timestamps chronologically instead of lexicographically —
/// lexicographic comparison silently gets it wrong when fractional
/// seconds appear on one side but not the other (e.g. cutoff
/// `2023-11-14T22:13:20Z` vs event `2023-11-14T22:13:20.5Z`: the event
/// sorts *before* the cutoff as strings because `.` < `Z` in ASCII,
/// even though it is chronologically *after*).
fn parse_rfc3339(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

/// Append-only writer. Each `append` flushes the line to disk and fsyncs the
/// file descriptor so replay is durable across crashes.
///
/// The on-disk file is opened lazily on the first `append` to avoid leaving
/// 0-byte orphans when a worker aborts at startup (e.g. ArtifactStore open
/// fails) before emitting any events — a bug observed in `.azoth/sessions/`
/// after failed TUI startups.
pub struct JsonlWriter {
    path: PathBuf,
    file: Option<BufWriter<File>>,
    tap: Option<UnboundedSender<SessionEvent>>,
    mirror: Option<SqliteMirror>,
}

impl JsonlWriter {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            path,
            file: None,
            tap: None,
            mirror: None,
        })
    }

    /// Open an existing session file for resume. Errors with `NotFound`
    /// if the file does not exist. **Does NOT run crash-recovery** —
    /// call [`recover_dangling`](Self::recover_dangling) explicitly
    /// after attaching `set_mirror` + `set_tap` so both observers see
    /// the synthetic terminal events.
    ///
    /// PR #18 round 7 (codex P2 3115635793): the prior behaviour —
    /// auto-recovery inside `open_existing` — emitted synthetic
    /// `TurnAborted { reason: Stalled }` / `TurnInterrupted`
    /// markers by writing directly to the file, bypassing
    /// [`set_mirror`](Self::set_mirror) and [`set_tap`](Self::set_tap)
    /// which are attached *after* `open_existing` returns. SQLite
    /// mirror rows drifted from JSONL on every resume until a full
    /// rebuild, and `/status` reported stale totals. Splitting open
    /// from recover lets callers wire mirror/tap first, then fan out
    /// the recovery events through [`append`](Self::append).
    pub fn open_existing<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("session file not found: {}", path.display()),
            ));
        }
        Self::open(path)
    }

    /// Close any dangling turns in the session file by appending a
    /// synthetic terminal marker for each. Routes every synthetic
    /// through [`append`](Self::append) so mirror + tap observers stay
    /// consistent with the file. Idempotent: a second call on a
    /// recovered file emits nothing.
    ///
    /// Call order: `set_mirror` → `recover_dangling` → `set_tap`. With
    /// that ordering, the mirror receives the synthetic events (fixing
    /// codex P2 3115635793) while the tap skips them — the TUI
    /// hydration path reads them from the JSONL scan instead, so the
    /// tap replay would otherwise double-insert them into the
    /// scrollback.
    pub fn recover_dangling(&mut self) -> io::Result<Vec<TurnId>> {
        // Make sure any buffered writes from prior `append` calls are
        // visible to the reader before we classify dangling turns —
        // otherwise a just-appended TurnStarted could be misread as
        // dangling. In practice this is defensive: callers only invoke
        // `recover_dangling` right after `open_existing`, before any
        // appends.
        if let Some(f) = self.file.as_mut() {
            f.flush()?;
            f.get_ref().sync_data()?;
        }

        let reader = JsonlReader::open(&self.path);
        let synthetics = reader
            .compute_dangling_synthetics()
            .map_err(io::Error::other)?;
        let mut ids = Vec::with_capacity(synthetics.len());
        for (tid, ev) in synthetics {
            self.append(&ev)?;
            ids.push(tid);
        }
        Ok(ids)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Attach an out-of-band listener that observes every appended event
    /// after the line has been durably flushed to disk. Used by the TUI to
    /// stream session events into the scrollback without reparsing the file.
    pub fn set_tap(&mut self, tap: UnboundedSender<SessionEvent>) {
        self.tap = Some(tap);
    }

    /// Attach a SQLite mirror. The mirror is updated in the same
    /// post-fsync position as the tap, so it never observes an event
    /// that isn't durable on disk. No-op for every variant except
    /// `RunStarted` / `TurnCommitted` / `TurnAborted` — see
    /// `SqliteMirror::apply`.
    pub fn set_mirror(&mut self, mirror: SqliteMirror) {
        self.mirror = Some(mirror);
    }

    pub fn append(&mut self, event: &SessionEvent) -> io::Result<()> {
        let line = serialize_line(event)?;
        let file = match &mut self.file {
            Some(f) => f,
            None => {
                let mut f = OpenOptions::new()
                    .create(true)
                    .read(true)
                    .append(true)
                    .open(&self.path)?;
                // PR #18 round 7 (self-audit sibling to gemini MED
                // 3115612841): `recover_dangling_turns` guards its
                // direct-write path against a partial prior write,
                // but `writer.recover_dangling` routes through this
                // append path on the first event, which would
                // concatenate the new line onto the partial tail.
                // Probe once on lazy-open via the shared helper. Cost:
                // one metadata() + one seek + one read, exactly once
                // per writer instance.
                ensure_newline_at_tail(&mut f)?;
                self.file = Some(BufWriter::new(f));
                self.file.as_mut().expect("just set")
            }
        };
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.get_ref().sync_data()?;
        if let Some(tap) = &self.tap {
            let _ = tap.send(event.clone());
        }
        if let Some(mirror) = &mut self.mirror {
            if let Err(e) = mirror.apply(event) {
                tracing::warn!(error = %e, "sqlite mirror apply failed");
            }
        }
        Ok(())
    }
}

fn serialize_line<T: Serialize>(event: &T) -> io::Result<String> {
    serde_json::to_string(event).map_err(io::Error::other)
}

/// Inject a trailing `\n` at the file tail if one is missing. Used by
/// both the direct-write recovery path and the writer's lazy-open path
/// to defend against partial prior writes (kernel persisted line bytes
/// but not the trailing newline before a crash) or external appenders
/// that don't terminate their lines.
///
/// Expects the file to be opened with both `read(true)` and
/// `append(true)`. `append` mode on Linux auto-positions writes to EOF
/// regardless of the seek cursor, so the seek here only affects the
/// subsequent read.
///
/// On any I/O error during the probe this function propagates;
/// callers that would rather skip the guard on read errors can map
/// the result. A `read` returning 0 bytes or an empty file short-
/// circuits cleanly — nothing to merge onto, nothing to fix.
fn ensure_newline_at_tail(file: &mut File) -> io::Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    match file.read(&mut last) {
        Ok(1) if last[0] != b'\n' => {
            file.write_all(b"\n")?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Which terminal marker closed a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcomeKind {
    Committed,
    Aborted,
    Interrupted,
}

/// Event as seen by the replayable projection — a thin newtype so callers
/// can't confuse it with the forensic variant at compile time.
#[derive(Debug, Clone)]
pub struct ReplayableEvent(pub SessionEvent);

/// Event as seen by the forensic projection, carrying its (non_)replayable
/// status.
#[derive(Debug, Clone)]
pub struct ForensicEvent {
    pub event: SessionEvent,
    pub non_replayable: bool,
}

/// Reader that materializes both projections over the same underlying file.
pub struct JsonlReader {
    path: PathBuf,
}

impl JsonlReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Scan the file once, grouping events by turn and classifying the
    /// outcome of each turn. Turns without any terminal marker are classified
    /// as `None` (dangling); the caller decides whether to patch them.
    ///
    /// Pub so the TUI resume path can scan once and fold via the
    /// [`Scan`] projection methods — see PR #18 round 7 (gemini MED
    /// 3115612857) for context on why this used to be four separate
    /// scans.
    pub fn scan(&self) -> Result<Scan, ProjectionError> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut events: Vec<SessionEvent> = Vec::new();
        let mut outcomes: HashMap<TurnId, TurnOutcomeKind> = HashMap::new();

        for (idx, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let ev: SessionEvent =
                serde_json::from_str(&line).map_err(|source| ProjectionError::Parse {
                    line: idx + 1,
                    source,
                })?;

            match &ev {
                SessionEvent::TurnCommitted { turn_id, .. } => {
                    outcomes.insert(turn_id.clone(), TurnOutcomeKind::Committed);
                }
                SessionEvent::TurnAborted { turn_id, .. } => {
                    outcomes.insert(turn_id.clone(), TurnOutcomeKind::Aborted);
                }
                SessionEvent::TurnInterrupted { turn_id, .. } => {
                    outcomes.insert(turn_id.clone(), TurnOutcomeKind::Interrupted);
                }
                _ => {}
            }
            events.push(ev);
        }

        Ok(Scan { events, outcomes })
    }

    /// Chronon CP-5 internal: the time-bounded sibling of `scan`.
    ///
    /// Two passes over the raw scan:
    ///
    /// 1. Compute an effective terminal timestamp per turn: `terminal.at`
    ///    if present, else the turn's `TurnStarted.timestamp` as a
    ///    charitable fallback for pre-CP-1 turns and crash-recovery
    ///    synthetics. A turn with no terminal marker at all drops out.
    ///
    /// 2. Keep only events that either belong to a visible turn, or that
    ///    are non-turn (`RunStarted` / `ContractAccepted`) with their own
    ///    `timestamp` ≤ `as_of`.
    ///
    /// Outcomes are filtered to the visible set so downstream
    /// `is_replayable` and consumers can't accidentally treat a dropped
    /// turn as committed.
    ///
    /// ## Performance profile (deferred optimisation, tracked in PR #18 rounds 4/5/6)
    ///
    /// Gemini raised multi-pass / intermediate-HashMap memory usage on
    /// three separate review rounds (comments 3114085221, 3114141532,
    /// 3114298726). The concerns are real at the *library* level but
    /// orthogonal to the v2.0.2 release gate:
    ///
    /// - JSONL is authoritative (CLAUDE.md CRIT-1). The m0007 SQLite
    ///   index (landed CP-5) mirrors `turn_started` + terminal markers
    ///   plus `turns.at`, but does **not** carry heartbeats, content
    ///   blocks, tool results, or effect records. Those events live only
    ///   in the JSONL stream, so `scan_as_of` has to load it to answer a
    ///   forensic query.
    /// - The two intermediate maps (`turn_started_at`, `terminal_at`)
    ///   are the simplest correct shape: terminal markers can appear
    ///   before or after the corresponding `TurnStarted` in log order
    ///   under parallel-turn futures, so a single-pass filter would
    ///   mis-classify turns whose terminal marker scans before their
    ///   opener. Visible-set construction is O(turns), not O(events).
    /// - Streaming projection wants a real benchmark harness and its
    ///   own PR — premature without a profile showing multi-pass as the
    ///   dominant cost. The upstream cost is the JSONL load itself
    ///   (BufReader + line parsing + serde); the in-memory folds are
    ///   constant-factor work over an already-materialised Vec.
    ///
    /// When/how to revisit: (a) `scan_as_of` shows up in a TUI hot path
    /// (currently only CLI `azoth resume --as-of` + rail rebuild on
    /// session open, neither per-frame) and (b) a new
    /// `turns_at_heartbeats` SQLite index lands so the forensic
    /// projection can ride the mirror instead of the JSONL. Either
    /// trigger flips this to a streaming impl; neither is true today.
    pub fn scan_as_of(&self, as_of: &str) -> Result<Scan, ProjectionError> {
        // Parse the cutoff once. A malformed `--as-of` is a user-facing
        // error, so surface it loudly rather than silently excluding
        // everything.
        let as_of_dt = parse_rfc3339(as_of).ok_or_else(|| ProjectionError::MalformedAsOf {
            input: as_of.to_string(),
            detail: "expected RFC3339 (e.g. 2026-04-20T10:00:00Z)".to_string(),
        })?;
        let le_as_of = |ts: &str| -> bool {
            // Unparseable event timestamps exclude the turn from visibility:
            // we can't prove chronological order, so conservative default
            // is "not yet visible at `as_of`". In practice every runtime
            // timestamp flows through `Clock::now_iso()` so parsing
            // succeeds for all well-formed sessions.
            parse_rfc3339(ts).map(|t| t <= as_of_dt).unwrap_or(false)
        };

        let raw = self.scan()?;

        // First pass: per-turn effective terminal `at`.
        let mut turn_started_at: HashMap<TurnId, String> = HashMap::new();
        let mut terminal_at: HashMap<TurnId, Option<String>> = HashMap::new();
        for ev in &raw.events {
            match ev {
                SessionEvent::TurnStarted {
                    turn_id, timestamp, ..
                } => {
                    turn_started_at
                        .entry(turn_id.clone())
                        .or_insert_with(|| timestamp.clone());
                }
                SessionEvent::TurnCommitted { turn_id, at, .. }
                | SessionEvent::TurnAborted { turn_id, at, .. }
                | SessionEvent::TurnInterrupted { turn_id, at, .. } => {
                    terminal_at.insert(turn_id.clone(), at.clone());
                }
                _ => {}
            }
        }

        // Determine visible turns: turns with a terminal marker whose
        // effective `at` (own `at`, else `TurnStarted.timestamp`) is
        // ≤ as_of. Comparison is chronological via parsed `OffsetDateTime`,
        // not lexicographic — sub-second precision varies across events
        // and the string-order fallback is silently wrong around the
        // second boundary.
        let mut visible: std::collections::HashSet<TurnId> = std::collections::HashSet::new();
        for (turn_id, maybe_at) in &terminal_at {
            let effective = maybe_at
                .as_deref()
                .or_else(|| turn_started_at.get(turn_id).map(String::as_str));
            if let Some(ts) = effective {
                if le_as_of(ts) {
                    visible.insert(turn_id.clone());
                }
            }
        }

        // Second pass: filter. Non-turn events gate on their own
        // `timestamp`; turn events ride their turn's visibility decision.
        let events: Vec<SessionEvent> = raw
            .events
            .into_iter()
            .filter(|ev| match ev {
                SessionEvent::RunStarted { timestamp, .. }
                | SessionEvent::ContractAccepted { timestamp, .. } => le_as_of(timestamp),
                _ => match ev.turn_id() {
                    Some(t) => visible.contains(t),
                    None => true,
                },
            })
            .collect();

        let outcomes: HashMap<TurnId, TurnOutcomeKind> = raw
            .outcomes
            .into_iter()
            .filter(|(t, _)| visible.contains(t))
            .collect();

        Ok(Scan { events, outcomes })
    }

    /// Replayable projection: only lines from turns whose terminal marker is
    /// `turn_committed`. Turns with any other outcome (aborted, interrupted,
    /// dangling) are dropped whole — making orphaned `tool_result` blocks
    /// structurally impossible on replay (CRIT-1).
    pub fn replayable(&self) -> Result<Vec<ReplayableEvent>, ProjectionError> {
        let scan = self.scan()?;
        Ok(scan
            .events
            .into_iter()
            .filter(|ev| is_replayable(ev, &scan.outcomes))
            .map(ReplayableEvent)
            .collect())
    }

    /// Chronon CP-5: bounded `replayable`. Same committed-only semantics,
    /// but the visible set is also gated on each turn's effective `at`
    /// being ≤ `as_of` (see [`scan_as_of`](Self::scan_as_of) for the
    /// visibility rule).
    pub fn replayable_as_of(&self, as_of: &str) -> Result<Vec<ReplayableEvent>, ProjectionError> {
        let scan = self.scan_as_of(as_of)?;
        Ok(scan
            .events
            .into_iter()
            .filter(|ev| is_replayable(ev, &scan.outcomes))
            .map(ReplayableEvent)
            .collect())
    }

    /// Forensic projection: every line, tagged with its replayability.
    pub fn forensic(&self) -> Result<Vec<ForensicEvent>, ProjectionError> {
        let scan = self.scan()?;
        Ok(scan
            .events
            .into_iter()
            .map(|ev| {
                let non_replayable = !is_replayable(&ev, &scan.outcomes);
                ForensicEvent {
                    event: ev,
                    non_replayable,
                }
            })
            .collect())
    }

    /// Chronon CP-5: forensic projection bounded by wall-clock `as_of`.
    ///
    /// A turn's events are included iff its terminal marker's `at` is
    /// ≤ `as_of`. For terminal markers without `at` (pre-CP-1 sessions
    /// or crash-recovery synthetics) the turn's `TurnStarted.timestamp`
    /// is the fallback — an honest approximation since such turns
    /// carry no precise end time. Turns without any terminal marker are
    /// always excluded: as of `as_of` they were mid-flight, so no
    /// `TurnCommitted` / `TurnAborted` / `TurnInterrupted` had landed
    /// yet (CRIT-1 atomicity, extended to the time axis).
    ///
    /// Non-turn events (`RunStarted`, `ContractAccepted`) gate on their
    /// own `timestamp` field; `TurnStarted` rides along with its turn's
    /// visibility decision.
    ///
    /// Timestamps compare chronologically via parsed `OffsetDateTime`
    /// (see [`scan_as_of`](Self::scan_as_of)) — the string-comparison
    /// shortcut would be silently wrong around the second boundary
    /// because sub-second precision in the emitted RFC3339 varies
    /// between events. An unparseable `as_of` surfaces as
    /// [`ProjectionError::MalformedAsOf`].
    ///
    /// The returned events carry the `non_replayable` tag under the
    /// same semantics as `forensic()` (committed-only = replayable).
    pub fn forensic_as_of(&self, as_of: &str) -> Result<Vec<ForensicEvent>, ProjectionError> {
        let scan = self.scan_as_of(as_of)?;
        Ok(scan
            .events
            .into_iter()
            .map(|ev| {
                let non_replayable = !is_replayable(&ev, &scan.outcomes);
                ForensicEvent {
                    event: ev,
                    non_replayable,
                }
            })
            .collect())
    }

    /// Bounded variant of [`last_accepted_contract`](Self::last_accepted_contract):
    /// returns the most recent contract accepted at or before `as_of`.
    pub fn last_accepted_contract_as_of(
        &self,
        as_of: &str,
    ) -> Result<Option<crate::schemas::Contract>, ProjectionError> {
        let scan = self.scan_as_of(as_of)?;
        Ok(scan.events.into_iter().rev().find_map(|ev| match ev {
            SessionEvent::ContractAccepted { contract, .. } => Some(contract),
            _ => None,
        }))
    }

    /// The most recently accepted contract, rehydrated from the last
    /// `ContractAccepted` event in the log. Returns `Ok(None)` if the session
    /// has never persisted one.
    pub fn last_accepted_contract(
        &self,
    ) -> Result<Option<crate::schemas::Contract>, ProjectionError> {
        let scan = self.scan()?;
        Ok(scan.events.into_iter().rev().find_map(|ev| match ev {
            SessionEvent::ContractAccepted { contract, .. } => Some(contract),
            _ => None,
        }))
    }

    /// β: the accepted contract with every replayable `ContractAmended`
    /// delta folded in. Returns `Ok(None)` if no contract was ever
    /// accepted. This is the view a resuming driver should bind to its
    /// `contract` field so budget checks start from the same effective
    /// ceiling the prior session saw at its last committed turn.
    ///
    /// Only amends inside committed turns count (same rule as
    /// `committed_run_progress`) — an amend whose turn later aborted is
    /// forensic-only and must not influence the live ceiling.
    ///
    /// Amends are matched against the accepted contract by `contract_id`
    /// AND by position: only amends that appear AFTER the last
    /// `ContractAccepted` event in the replayable stream are folded.
    /// A mid-session contract replacement supersedes prior amends
    /// regardless of id-collision: a fresh contract starts with its own
    /// budget, unaffected by any amends issued against an earlier
    /// acceptance of the same id (defense-in-depth).
    ///
    /// R1 (gemini PR #31 HIGH): single-pass implementation. The prior
    /// version called `last_accepted_contract()` + `replayable()`,
    /// each of which re-parsed the entire JSONL file. This now uses a
    /// single `scan()` and walks the already-materialised replayable
    /// projection once.
    pub fn last_effective_contract(
        &self,
    ) -> Result<Option<crate::schemas::Contract>, ProjectionError> {
        let replay = self.scan()?.replayable();
        // First pass: find the last `ContractAccepted` index.
        let last_accepted_idx =
            replay
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, ReplayableEvent(ev))| match ev {
                    SessionEvent::ContractAccepted { .. } => Some(i),
                    _ => None,
                });
        let Some(start) = last_accepted_idx else {
            return Ok(None);
        };
        let mut contract = match &replay[start].0 {
            SessionEvent::ContractAccepted { contract, .. } => contract.clone(),
            // Unreachable: `start` comes from a position we just
            // classified as `ContractAccepted` above. The arm exists
            // to keep the match exhaustive without an `if let`
            // wrapper that fights the `.clone()` path.
            _ => unreachable!("last_accepted_idx points to a ContractAccepted"),
        };
        // Second pass (over the slice AFTER the acceptance): collect
        // amends whose contract_id matches. Slice is bounded — not
        // a second file read.
        let amends: Vec<crate::schemas::EffectBudgetDelta> = replay[start + 1..]
            .iter()
            .filter_map(|ReplayableEvent(ev)| match ev {
                SessionEvent::ContractAmended {
                    contract_id, delta, ..
                } if *contract_id == contract.id => Some(delta.clone()),
                _ => None,
            })
            .collect();
        crate::contract::apply_amends(&mut contract, &amends);
        Ok(Some(contract))
    }

    /// Recompute `(EffectCounter, turns_completed)` from the replayable
    /// projection so a resuming worker can seed the contract gates exactly
    /// as if it had been the one driving the prior turns. Effects inside
    /// aborted or interrupted turns are excluded — the live path bumps the
    /// counter only after `EffectRecord` is durably appended and the turn
    /// goes on to commit, so replay must match that accounting.
    ///
    /// `network_reads` stays zero: the live driver doesn't bump it either
    /// (no v1 tool currently maps to it); recomputing it from JSONL must
    /// not drift from the runtime path.
    pub fn committed_run_progress(&self) -> Result<(EffectCounter, u32), ProjectionError> {
        let replay = self.replayable()?;
        Self::fold_progress(replay.into_iter().map(|ReplayableEvent(ev)| ev))
    }

    /// Chronon CP-5: bounded `committed_run_progress`. Counts effects and
    /// committed turns only over turns whose terminal marker is
    /// `TurnCommitted` AND whose effective `at` is ≤ `as_of`.
    pub fn committed_run_progress_as_of(
        &self,
        as_of: &str,
    ) -> Result<(EffectCounter, u32), ProjectionError> {
        let scan = self.scan_as_of(as_of)?;
        // Only the committed-outcome subset counts toward resume budgets.
        let committed_only = scan.events.into_iter().filter(|ev| match ev.turn_id() {
            None => true,
            Some(t) => matches!(scan.outcomes.get(t), Some(TurnOutcomeKind::Committed)),
        });
        Self::fold_progress(committed_only)
    }

    fn fold_progress<I: IntoIterator<Item = SessionEvent>>(
        events: I,
    ) -> Result<(EffectCounter, u32), ProjectionError> {
        let mut effects = EffectCounter::default();
        let mut turns_completed: u32 = 0;
        // R1 (gemini HIGH + codex P2): track the currently-active
        // contract id so amends that target an older, superseded
        // contract do NOT inflate the ceiling bonus for the new one.
        // A mid-session `ContractAccepted` resets the ceiling-bonus
        // triplet because those bonuses applied to the prior contract
        // and the new contract starts its ceiling fresh.
        //
        // R2 (codex PR #31 P2): `amends_this_run` is the per-run
        // brake counter. Resetting it on ContractAccepted would let
        // a user bypass `MAX_AMENDS_PER_RUN` by cycling contracts.
        // The brake is run-scope, stricter than contract-scope — so
        // the run-amend count accumulates across contract boundaries
        // even when the bonus magnitudes do not.
        //
        // `apply_local` / `apply_repo` tallies are intentionally NOT
        // reset here — that is pre-existing behaviour (pre-β) and
        // changing it would ripple into `resume_recomputes_effects_
        // and_turns`. Separate concern from the β amend bug.
        let mut current_contract_id: Option<crate::schemas::ContractId> = None;
        for ev in events {
            match ev {
                SessionEvent::ContractAccepted { contract, .. } => {
                    current_contract_id = Some(contract.id);
                    effects.apply_local_ceiling_bonus = 0;
                    effects.apply_repo_ceiling_bonus = 0;
                    effects.network_reads_ceiling_bonus = 0;
                    // amends_this_run intentionally preserved — see
                    // R2 comment above.
                }
                // β: fold ContractAmended deltas into the ceiling-bonus
                // fields and bump the run-scoped amend counter. Only
                // folds amends that target the currently-active
                // contract — defensive against mid-session contract
                // replacement (gemini PR #31 R0 HIGH / codex P2).
                SessionEvent::ContractAmended {
                    contract_id, delta, ..
                } => {
                    if current_contract_id.as_ref() != Some(&contract_id) {
                        continue;
                    }
                    effects.apply_local_ceiling_bonus = effects
                        .apply_local_ceiling_bonus
                        .saturating_add(delta.apply_local);
                    effects.apply_repo_ceiling_bonus = effects
                        .apply_repo_ceiling_bonus
                        .saturating_add(delta.apply_repo);
                    effects.network_reads_ceiling_bonus = effects
                        .network_reads_ceiling_bonus
                        .saturating_add(delta.network_reads);
                    effects.amends_this_run = effects.amends_this_run.saturating_add(1);
                    // amends_this_turn intentionally NOT restored — it
                    // is per-turn state that drive_turn resets on entry;
                    // resuming is always the start of a fresh turn so
                    // carrying a stale per-turn tally would double-count
                    // brake pressure.
                }
                SessionEvent::EffectRecord { effect, .. } => match effect.class {
                    EffectClass::ApplyLocal => {
                        effects.apply_local = effects.apply_local.saturating_add(1);
                    }
                    EffectClass::ApplyRepo => {
                        effects.apply_repo = effects.apply_repo.saturating_add(1);
                    }
                    _ => {}
                },
                SessionEvent::TurnCommitted { .. } => {
                    turns_completed = turns_completed.saturating_add(1);
                }
                _ => {}
            }
        }
        Ok((effects, turns_completed))
    }

    /// Rebuild the cross-turn `Vec<Message>` a live worker would have in
    /// memory after the prior session's committed turns, reading *only* from
    /// the replayable projection. For each `TurnCommitted` that carries the
    /// `user_input` + `final_assistant` rehydrate fields (introduced in v1.5),
    /// pushes a `Role::User` message followed by a `Role::Assistant` message.
    /// Turns committed before v1.5 — or with either field absent — are
    /// skipped whole so a restarted worker never feeds a partial exchange
    /// back to the model.
    ///
    /// The live worker path in v1.5 pushes `(user, assistant)` into history
    /// after every `TurnCommitted`; this method is the replay mirror of that
    /// same sequence and is what keeps `/resume` from hitting total amnesia.
    pub fn rebuild_history(&self) -> Result<Vec<Message>, ProjectionError> {
        let replay = self.replayable()?;
        Ok(Self::fold_history(
            replay.into_iter().map(|ReplayableEvent(ev)| ev),
        ))
    }

    /// Chronon CP-5: bounded `rebuild_history`. Rehydrates the cross-turn
    /// `Vec<Message>` from the as-of-committed subset, so resume under
    /// `--as-of` feeds the model exactly the history that was in memory
    /// at `as_of`.
    pub fn rebuild_history_as_of(&self, as_of: &str) -> Result<Vec<Message>, ProjectionError> {
        let scan = self.scan_as_of(as_of)?;
        let committed_only = scan.events.into_iter().filter(|ev| match ev.turn_id() {
            None => true,
            Some(t) => matches!(scan.outcomes.get(t), Some(TurnOutcomeKind::Committed)),
        });
        Ok(Self::fold_history(committed_only))
    }

    fn fold_history<I: IntoIterator<Item = SessionEvent>>(events: I) -> Vec<Message> {
        let mut history: Vec<Message> = Vec::new();
        for ev in events {
            if let SessionEvent::TurnCommitted {
                user_input: Some(user),
                final_assistant: Some(assistant),
                ..
            } = ev
            {
                history.push(Message {
                    role: Role::User,
                    content: user,
                });
                history.push(Message {
                    role: Role::Assistant,
                    content: assistant,
                });
            }
        }
        history
    }

    /// Crash recovery: scan for turns with no terminal marker and append a
    /// synthetic terminal record for each. Idempotent.
    ///
    /// Chronon CP-2 refinement: a dangling turn that emitted at least one
    /// heartbeat is reclassified as `TurnAborted { reason: Stalled }` —
    /// carrying the last-known heartbeat timestamp as `at`. A dangling
    /// turn with no heartbeat stays `TurnInterrupted { reason: Crash }`:
    /// the runtime literally has no temporal evidence of when the turn
    /// last made progress.
    pub fn recover_dangling_turns(&self) -> Result<Vec<TurnId>, ProjectionError> {
        let synthetics = self.compute_dangling_synthetics()?;
        if synthetics.is_empty() {
            return Ok(Vec::new());
        }

        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)?;

        // PR #18 round 7 (gemini MED 3115612841): the prior
        // `seek(End(0)) + write_all(line) + write_all(b"\n")` sequence
        // could produce a corrupted JSONL if the prior write ended
        // without a trailing newline — either because a crash hit
        // between the two `write_all` calls (kernel pages may flush
        // the line bytes without the trailing `\n`), or because some
        // external producer wrote to the file. The first synthetic
        // marker would concatenate to the partial prior line, giving
        // `{"type":"x"}{"type":"synthetic"}\n` which `serde_json`
        // refuses to parse — the NEXT resume crashes at scan(). Guard
        // once via the shared helper, which reads the last byte and
        // injects `\n` if missing.
        ensure_newline_at_tail(&mut file)?;
        // `append` mode ignores this seek for writes but keeps reads
        // pointing at EOF for symmetry.
        file.seek(SeekFrom::End(0))?;
        for (_, synthetic) in &synthetics {
            let line = serialize_line(synthetic)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        // PR #18 round 6 (gemini MED 3114298744 — rejected with docs,
        // NOT a fix): gemini flagged "markers not explicitly flushed or
        // synced; consider sync_all". Empirically verified: `sync_data`
        // IS called here, which maps to POSIX `fdatasync(2)`. For
        // append-only JSONL the on-disk distinction is:
        //   - `sync_data` / fdatasync → data blocks + essential metadata
        //     (file size — needed to read the appended bytes back)
        //   - `sync_all` / fsync → above + all metadata (mtime, atime)
        // Recovery markers are durable after `sync_data`: a subsequent
        // process can `File::open` the file, read its full (grown) size,
        // and scan every appended byte. `sync_all` would add mtime
        // persistence which we don't depend on for correctness. Keeping
        // `sync_data` avoids the extra seek on the inode metadata block
        // on every recovery path. Symmetric with the main-append path
        // at line ~136 which also uses `sync_data`.
        file.sync_data()?;
        Ok(synthetics.into_iter().map(|(tid, _)| tid).collect())
    }

    /// Pure computation half of [`recover_dangling_turns`](Self::recover_dangling_turns):
    /// scan the file, classify dangling turns, and return the synthetic
    /// terminal events they'd be closed with. No I/O beyond `scan()`.
    ///
    /// PR #18 round 7 (codex P2 3115635793): the TUI resume path needs
    /// the *writer* to emit these synthetics through `JsonlWriter::append`
    /// so that the SQLite mirror and the UI tap see them. Factoring the
    /// classification out lets [`JsonlWriter::recover_dangling`] reuse
    /// the same logic while going through `append` for durability +
    /// side-effect fan-out.
    ///
    /// The returned `Vec<(TurnId, SessionEvent)>` is paired so callers
    /// can report both the recovered turn IDs and the exact events that
    /// landed on disk.
    pub fn compute_dangling_synthetics(
        &self,
    ) -> Result<Vec<(TurnId, SessionEvent)>, ProjectionError> {
        let scan = self.scan()?;
        // Index the last seen heartbeat per turn. Walk events forward so
        // the final assignment is the most recent heartbeat — same shape
        // as the outcomes map. We carry `tokens_out` alongside the
        // timestamp so the synthetic Stalled record can preserve the
        // last-known token usage (runtime session budgets stay accurate
        // after crash recovery instead of silently resetting to zero).
        let mut last_heartbeat: HashMap<TurnId, (String, u64)> = HashMap::new();
        for ev in &scan.events {
            if let SessionEvent::TurnHeartbeat {
                turn_id,
                at,
                progress,
            } = ev
            {
                if !at.is_empty() {
                    last_heartbeat.insert(turn_id.clone(), (at.clone(), progress.tokens_out));
                }
            }
        }

        let mut dangling: Vec<TurnId> = Vec::new();
        for ev in &scan.events {
            if let SessionEvent::TurnStarted { turn_id, .. } = ev {
                if !scan.outcomes.contains_key(turn_id) && !dangling.contains(turn_id) {
                    dangling.push(turn_id.clone());
                }
            }
        }

        let mut out: Vec<(TurnId, SessionEvent)> = Vec::with_capacity(dangling.len());
        for turn_id in dangling {
            let synthetic = match last_heartbeat.get(&turn_id) {
                Some((hb_at, hb_tokens)) => {
                    // Heartbeat evidence exists → reclassify as Stalled.
                    // The `at` field carries the last heartbeat, not
                    // "now" — operators need to see when progress
                    // actually stopped, not when resume noticed.
                    //
                    // Preserve the last observed `tokens_out` from the
                    // heartbeat. Usage is cumulative per turn, so the
                    // final heartbeat is the closest honest estimate
                    // of what the model actually emitted before
                    // stalling. Saturating cast to u32: a single turn
                    // emitting >4.2B tokens is outside any realistic
                    // budget, clamping is safer than panicking. When
                    // the clamp actually triggers we log it so operators
                    // know the recovered session's token accounting is
                    // no longer precise — silent clamping is a known
                    // silent-failure antipattern and explicitly out of
                    // step with the durable-evidence invariant (#5).
                    let output_tokens = u32::try_from(*hb_tokens).unwrap_or_else(|_| {
                        tracing::warn!(
                            turn_id = %turn_id.0,
                            original_tokens_out = *hb_tokens,
                            clamped_to = u32::MAX,
                            "heartbeat tokens_out exceeded u32::MAX during crash \
                             recovery; clamping — recovered Stalled record is imprecise"
                        );
                        u32::MAX
                    });
                    SessionEvent::TurnAborted {
                        turn_id: turn_id.clone(),
                        reason: AbortReason::Stalled,
                        detail: Some(format!("last heartbeat at {hb_at}")),
                        usage: Usage {
                            output_tokens,
                            ..Usage::default()
                        },
                        at: Some(hb_at.clone()),
                    }
                }
                None => SessionEvent::TurnInterrupted {
                    turn_id: turn_id.clone(),
                    reason: AbortReason::Crash,
                    partial_usage: Default::default(),
                    // Honest `None`: no heartbeat, no idea when the crash
                    // happened. Emitting "now" would be misleading.
                    at: None,
                },
            };
            out.push((turn_id, synthetic));
        }
        Ok(out)
    }
}

/// Result of a single file scan, materialised once so callers can derive
/// multiple projections without re-reading the JSONL.
///
/// PR #18 round 7 (gemini MED 3115612857): the TUI resume path used to
/// call four `*_as_of` methods back-to-back, each of which called
/// `scan_as_of` internally and re-read the whole file. On a 250MB
/// session log that quadrupled boot time. The pub type + projection
/// methods below let the hydration path scan once and fold N times.
pub struct Scan {
    pub(crate) events: Vec<SessionEvent>,
    pub(crate) outcomes: HashMap<TurnId, TurnOutcomeKind>,
}

impl Scan {
    /// Replayable projection over the already-materialised events.
    pub fn replayable(&self) -> Vec<ReplayableEvent> {
        self.events
            .iter()
            .filter(|ev| is_replayable(ev, &self.outcomes))
            .cloned()
            .map(ReplayableEvent)
            .collect()
    }

    /// Forensic projection over the already-materialised events.
    pub fn forensic(&self) -> Vec<ForensicEvent> {
        self.events
            .iter()
            .map(|ev| ForensicEvent {
                event: ev.clone(),
                non_replayable: !is_replayable(ev, &self.outcomes),
            })
            .collect()
    }

    /// Most recent `ContractAccepted` in the scan, or `None` if the scan
    /// has never carried one.
    pub fn last_accepted_contract(&self) -> Option<crate::schemas::Contract> {
        self.events.iter().rev().find_map(|ev| match ev {
            SessionEvent::ContractAccepted { contract, .. } => Some(contract.clone()),
            _ => None,
        })
    }

    /// Recompute `(EffectCounter, turns_completed)` from committed turns
    /// only — same accounting the live driver produced.
    pub fn committed_run_progress(&self) -> (EffectCounter, u32) {
        let committed = self
            .events
            .iter()
            .filter(|ev| match ev.turn_id() {
                None => true,
                Some(t) => matches!(self.outcomes.get(t), Some(TurnOutcomeKind::Committed)),
            })
            .cloned();
        JsonlReader::fold_progress(committed).unwrap_or_default()
    }

    /// Rehydrate the cross-turn `Vec<Message>` from committed turns only.
    pub fn rebuild_history(&self) -> Vec<Message> {
        let committed = self
            .events
            .iter()
            .filter(|ev| match ev.turn_id() {
                None => true,
                Some(t) => matches!(self.outcomes.get(t), Some(TurnOutcomeKind::Committed)),
            })
            .cloned();
        JsonlReader::fold_history(committed)
    }

    /// True iff a `RunStarted` event has already been recorded in the
    /// events of this scan. Single-pass over `self.events`.
    pub fn has_run_started(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e, SessionEvent::RunStarted { .. }))
    }
}

/// An event is replayable iff it either has no turn (RunStarted) or it
/// belongs to a turn whose terminal marker is `TurnCommitted`.
fn is_replayable(ev: &SessionEvent, outcomes: &HashMap<TurnId, TurnOutcomeKind>) -> bool {
    match ev.turn_id() {
        None => true,
        Some(t) => matches!(outcomes.get(t), Some(TurnOutcomeKind::Committed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::{
        ArtifactId, CommitOutcome, ContentBlock, ContractId, RunId, ToolUseId, Usage,
    };
    use tempfile::tempdir;

    fn ts() -> String {
        "2026-04-15T12:00:00Z".to_string()
    }

    #[test]
    fn dual_projection_drops_non_committed_turns_from_replay() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut w = JsonlWriter::open(&path).unwrap();

        let run_id = RunId::from("run_abc".to_string());
        let contract_id = ContractId::from("ctr_xyz".to_string());
        let t1 = TurnId::from("t_001".to_string()); // committed
        let t2 = TurnId::from("t_002".to_string()); // aborted
        let t3 = TurnId::from("t_003".to_string()); // dangling

        w.append(&SessionEvent::RunStarted {
            run_id: run_id.clone(),
            contract_id,
            timestamp: ts(),
        })
        .unwrap();

        // committed turn
        w.append(&SessionEvent::TurnStarted {
            turn_id: t1.clone(),
            run_id: run_id.clone(),
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();
        w.append(&SessionEvent::ContentBlock {
            turn_id: t1.clone(),
            index: 0,
            block: ContentBlock::Text {
                text: "hello".into(),
            },
        })
        .unwrap();
        w.append(&SessionEvent::ToolResult {
            turn_id: t1.clone(),
            tool_use_id: ToolUseId::from("tu_a".to_string()),
            is_error: false,
            content_artifact: Some(ArtifactId::from("art_1".to_string())),
            call_group: None,
        })
        .unwrap();
        w.append(&SessionEvent::TurnCommitted {
            turn_id: t1.clone(),
            outcome: CommitOutcome::Success,
            usage: Usage::default(),
            user_input: None,
            final_assistant: None,
            at: None,
        })
        .unwrap();

        // aborted turn with a leftover tool_use block
        w.append(&SessionEvent::TurnStarted {
            turn_id: t2.clone(),
            run_id: run_id.clone(),
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();
        w.append(&SessionEvent::ContentBlock {
            turn_id: t2.clone(),
            index: 0,
            block: ContentBlock::ToolUse {
                id: ToolUseId::from("tu_b".to_string()),
                name: "repo_search".into(),
                input: serde_json::json!({"q":"x"}),
                call_group: None,
            },
        })
        .unwrap();
        w.append(&SessionEvent::TurnAborted {
            turn_id: t2.clone(),
            reason: AbortReason::ValidatorFail,
            detail: Some("impact_tests".into()),
            usage: Usage::default(),
            at: None,
        })
        .unwrap();

        // dangling turn — no terminal marker
        w.append(&SessionEvent::TurnStarted {
            turn_id: t3.clone(),
            run_id: run_id.clone(),
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();

        // Re-read.
        let r = JsonlReader::open(&path);
        let replay = r.replayable().unwrap();
        // Expect: RunStarted + exactly t1's four events.
        assert_eq!(
            replay.len(),
            5,
            "replay: {:#?}",
            replay.iter().map(|e| &e.0).collect::<Vec<_>>()
        );
        for e in &replay {
            match e.0.turn_id() {
                None => {}
                Some(tid) => assert_eq!(tid, &t1),
            }
        }

        let forensic = r.forensic().unwrap();
        let non_repl = forensic.iter().filter(|f| f.non_replayable).count();
        assert!(
            non_repl >= 4,
            "expected non_replayable tags on t2+t3, got {non_repl}"
        );
    }

    #[test]
    fn recover_dangling_turns_appends_interrupted_marker() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut w = JsonlWriter::open(&path).unwrap();

        let run_id = RunId::from("run_abc".to_string());
        let contract_id = ContractId::from("ctr_xyz".to_string());
        let t1 = TurnId::from("t_100".to_string());
        w.append(&SessionEvent::RunStarted {
            run_id: run_id.clone(),
            contract_id,
            timestamp: ts(),
        })
        .unwrap();
        w.append(&SessionEvent::TurnStarted {
            turn_id: t1.clone(),
            run_id,
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();
        drop(w);

        let r = JsonlReader::open(&path);
        let recovered = r.recover_dangling_turns().unwrap();
        assert_eq!(recovered, vec![t1.clone()]);

        // Re-running is idempotent: nothing left to recover.
        let again = r.recover_dangling_turns().unwrap();
        assert!(again.is_empty());

        let forensic = r.forensic().unwrap();
        assert!(forensic.iter().any(|f| matches!(
            &f.event,
            SessionEvent::TurnInterrupted {
                reason: AbortReason::Crash,
                ..
            }
        )));
    }

    #[test]
    fn recover_dangling_stalled_preserves_last_heartbeat_output_tokens() {
        // A dangling turn with heartbeat evidence must reclassify as
        // Stalled AND preserve the last observed `tokens_out` — otherwise
        // a resuming worker that reads aggregate usage loses the tokens
        // the model actually emitted before the stall.
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut w = JsonlWriter::open(&path).unwrap();

        let run_id = RunId::from("run_hb".to_string());
        let contract_id = ContractId::from("ctr_hb".to_string());
        let t1 = TurnId::from("t_hb".to_string());
        w.append(&SessionEvent::RunStarted {
            run_id: run_id.clone(),
            contract_id,
            timestamp: ts(),
        })
        .unwrap();
        w.append(&SessionEvent::TurnStarted {
            turn_id: t1.clone(),
            run_id,
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();
        // Two heartbeats — the later one wins.
        w.append(&SessionEvent::TurnHeartbeat {
            turn_id: t1.clone(),
            at: "2026-04-20T10:00:00Z".to_string(),
            progress: crate::schemas::HeartbeatProgress {
                content_blocks: 1,
                tool_calls: 0,
                tokens_out: 150,
            },
        })
        .unwrap();
        w.append(&SessionEvent::TurnHeartbeat {
            turn_id: t1.clone(),
            at: "2026-04-20T10:00:05Z".to_string(),
            progress: crate::schemas::HeartbeatProgress {
                content_blocks: 2,
                tool_calls: 1,
                tokens_out: 420,
            },
        })
        .unwrap();
        drop(w);

        let r = JsonlReader::open(&path);
        let recovered = r.recover_dangling_turns().unwrap();
        assert_eq!(recovered, vec![t1.clone()]);

        let forensic = r.forensic().unwrap();
        let stalled = forensic
            .iter()
            .find_map(|f| match &f.event {
                SessionEvent::TurnAborted {
                    reason: AbortReason::Stalled,
                    usage,
                    at,
                    ..
                } => Some((usage.output_tokens, at.clone())),
                _ => None,
            })
            .expect("synthetic TurnAborted{Stalled} should be present");

        assert_eq!(
            stalled.0, 420,
            "output_tokens should come from latest heartbeat"
        );
        assert_eq!(
            stalled.1.as_deref(),
            Some("2026-04-20T10:00:05Z"),
            "at should be the latest heartbeat's timestamp"
        );
    }

    #[test]
    fn open_without_append_does_not_create_file() {
        // A worker that aborts at startup (e.g. ArtifactStore::open fails)
        // before emitting any event must not leave a 0-byte orphan in
        // .azoth/sessions/. Observed previously as run_7df1b3bf6cd5.jsonl
        // at 0 bytes after a crashed TUI startup.
        let dir = tempdir().unwrap();
        let path = dir.path().join("deeper").join("session.jsonl");
        let w = JsonlWriter::open(&path).unwrap();
        assert!(
            !path.exists(),
            "open() must not touch disk; file created only on first append"
        );
        // Parent dir is still created eagerly — it's idempotent and cheap,
        // and ensures the subsequent append() can't fail for that reason.
        assert!(path.parent().unwrap().is_dir());
        drop(w);
        assert!(!path.exists(), "drop without append still leaves no file");
    }

    #[test]
    fn first_append_creates_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut w = JsonlWriter::open(&path).unwrap();
        assert!(!path.exists());
        w.append(&SessionEvent::RunStarted {
            run_id: RunId::from("run_x".to_string()),
            contract_id: ContractId::from("ctr_x".to_string()),
            timestamp: ts(),
        })
        .unwrap();
        assert!(path.exists(), "first append creates the file");
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes > 0);
    }

    #[test]
    fn open_existing_is_pure_open_without_recovery() {
        // PR #18 round 7 (codex P2 3115635793): `open_existing` no
        // longer auto-runs recovery. Mirror/tap-aware callers must
        // invoke `recover_dangling` explicitly so both observers see
        // the synthetic terminal events.
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut w = JsonlWriter::open(&path).unwrap();

        let run_id = RunId::from("run_resume".to_string());
        let contract_id = ContractId::from("ctr_resume".to_string());
        let t1 = TurnId::from("t_dangling".to_string());
        w.append(&SessionEvent::RunStarted {
            run_id: run_id.clone(),
            contract_id,
            timestamp: ts(),
        })
        .unwrap();
        w.append(&SessionEvent::TurnStarted {
            turn_id: t1,
            run_id,
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();
        drop(w);

        // Missing-file path: surfaces NotFound.
        let missing = dir.path().join("nope.jsonl");
        let err = JsonlWriter::open_existing(&missing).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        // `open_existing` is pure open now: no recovery, no synthetic
        // markers on disk.
        let w1 = JsonlWriter::open_existing(&path).unwrap();
        drop(w1);
        let count_after_open = JsonlReader::open(&path)
            .forensic()
            .unwrap()
            .iter()
            .filter(|f| matches!(&f.event, SessionEvent::TurnInterrupted { .. }))
            .count();
        assert_eq!(count_after_open, 0);

        // Explicit recovery: first call appends exactly one
        // TurnInterrupted, second call is idempotent (empty return).
        let mut w2 = JsonlWriter::open_existing(&path).unwrap();
        let recovered_first = w2.recover_dangling().unwrap();
        assert_eq!(recovered_first.len(), 1);
        let recovered_second = w2.recover_dangling().unwrap();
        assert!(recovered_second.is_empty());
        drop(w2);

        let count_after_recovery = JsonlReader::open(&path)
            .forensic()
            .unwrap()
            .iter()
            .filter(|f| matches!(&f.event, SessionEvent::TurnInterrupted { .. }))
            .count();
        assert_eq!(count_after_recovery, 1);
    }

    #[test]
    fn recover_dangling_through_writer_prepends_newline_if_tail_missing() {
        // PR #18 round 7 (gemini MED 3115612841): if the last byte of
        // the file is not `\n` (partial prior write that crashed
        // between `write_all(line)` and `write_all(b"\n")`, or an
        // external producer that didn't terminate its line),
        // appending a synthetic marker must NOT concatenate with the
        // partial prior line. The pre-fix code produced
        // `{"type":"turn_started"}{"type":"turn_interrupted"}\n` on
        // one line — unparseable by `serde_json`, breaking the next
        // resume.
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        // Manually construct a file whose tail lacks a trailing
        // newline. A simple way: write one line via the writer, then
        // append a partial line directly.
        let mut w = JsonlWriter::open(&path).unwrap();
        let run_id = RunId::from("run_partial".to_string());
        let contract_id = ContractId::from("ctr_partial".to_string());
        w.append(&SessionEvent::RunStarted {
            run_id: run_id.clone(),
            contract_id,
            timestamp: ts(),
        })
        .unwrap();
        drop(w);

        // Append a valid TurnStarted line WITHOUT a trailing newline.
        let t1 = TurnId::from("t_partial".to_string());
        let partial = SessionEvent::TurnStarted {
            turn_id: t1,
            run_id,
            parent_turn: None,
            timestamp: ts(),
        };
        let partial_line = serialize_line(&partial).unwrap();
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(partial_line.as_bytes()).unwrap();
            // Deliberately no trailing `\n` — simulates crash between
            // the two `write_all` calls in `append`.
            f.sync_data().unwrap();
        }

        // Recovery must inject the missing `\n` before appending the
        // synthetic marker, so both lines stay parseable.
        let mut w2 = JsonlWriter::open_existing(&path).unwrap();
        let recovered = w2.recover_dangling().unwrap();
        assert_eq!(recovered.len(), 1, "should recover the partial turn");
        drop(w2);

        // Re-scan: every line must parse. Pre-fix this panicked at
        // `scan()` because line 2 was `{..}{..}\n` — two concatenated
        // objects that `serde_json::from_str` rejects.
        let forensic = JsonlReader::open(&path).forensic().unwrap();
        assert!(
            forensic
                .iter()
                .any(|f| matches!(&f.event, SessionEvent::TurnStarted { .. })),
            "TurnStarted must survive the partial-line fix"
        );
        assert!(
            forensic
                .iter()
                .any(|f| matches!(&f.event, SessionEvent::TurnInterrupted { .. })),
            "synthetic TurnInterrupted must be on its own parseable line"
        );
    }

    #[test]
    fn append_first_event_injects_newline_if_file_tail_missing_it() {
        // PR #18 round 7 (self-audit sibling of gemini MED 3115612841):
        // if a prior crashed writer left the file with a partial
        // trailing line (no `\n`), the next `append` must NOT
        // concatenate — it must inject the missing newline first.
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        // Seed: valid line, then a partial trailing line with no `\n`.
        let run_id = RunId::from("run_probe".to_string());
        let contract_id = ContractId::from("ctr_probe".to_string());
        let first = SessionEvent::RunStarted {
            run_id: run_id.clone(),
            contract_id,
            timestamp: ts(),
        };
        {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(serialize_line(&first).unwrap().as_bytes())
                .unwrap();
            f.write_all(b"\n").unwrap();
            // Partial second line, crash simulation.
            f.write_all(b"{\"type\":\"partial\"").unwrap();
            f.sync_data().unwrap();
        }

        // Open writer and append a valid event. The lazy-open probe
        // must inject a `\n` before our new line, so the file parses
        // line-by-line cleanly (modulo the partial line, which is
        // still invalid JSON but at least on its own line).
        let mut w = JsonlWriter::open(&path).unwrap();
        let t1 = TurnId::from("t_probe".to_string());
        w.append(&SessionEvent::TurnStarted {
            turn_id: t1.clone(),
            run_id,
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();
        drop(w);

        // Read the file as raw bytes and assert the new event is on
        // its own line.
        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert!(
            lines.iter().any(|l| l.contains(&t1.0)),
            "new event must land on its own parseable line"
        );
        // The partial line must NOT have been merged with the new
        // event. Find the line(s) containing `t_probe` and check
        // neither contains the partial prefix.
        for line in &lines {
            if line.contains(&t1.0) {
                assert!(
                    !line.contains("partial"),
                    "append-path newline guard failed — line concatenated: {line}"
                );
            }
        }
    }

    #[test]
    fn recover_dangling_through_writer_fires_mirror() {
        // PR #18 round 7 (codex P2 3115635793): routing recovery
        // through `self.append` hits the mirror tap. This test uses a
        // SqliteMirror and asserts the synthetic TurnInterrupted lands
        // in the mirror's `turns` table.
        let dir = tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let db_path = dir.path().join("state.sqlite");

        // Seed: RunStarted + TurnStarted + Heartbeat, no terminal →
        // dangling turn with heartbeat evidence. Recovery
        // reclassifies this as TurnAborted{Stalled} (NOT
        // TurnInterrupted), which is the mirrored variant —
        // SqliteMirror::apply is a no-op for TurnInterrupted by
        // design, so a heartbeat-less dangling turn wouldn't exercise
        // the mirror path anyway.
        let mut w = JsonlWriter::open(&path).unwrap();
        let run_id = RunId::from("run_mirror".to_string());
        let contract_id = ContractId::from("ctr_mirror".to_string());
        let t1 = TurnId::from("t_mirror_dangling".to_string());
        w.append(&SessionEvent::RunStarted {
            run_id: run_id.clone(),
            contract_id,
            timestamp: ts(),
        })
        .unwrap();
        w.append(&SessionEvent::TurnStarted {
            turn_id: t1.clone(),
            run_id,
            parent_turn: None,
            timestamp: ts(),
        })
        .unwrap();
        w.append(&SessionEvent::TurnHeartbeat {
            turn_id: t1.clone(),
            at: ts(),
            progress: crate::schemas::HeartbeatProgress {
                content_blocks: 1,
                tool_calls: 0,
                tokens_out: 42,
            },
        })
        .unwrap();
        drop(w);

        // Resume: attach mirror FIRST, then recover. The mirror must
        // observe the synthetic TurnAborted{Stalled} through
        // `append`.
        let mut w2 = JsonlWriter::open_existing(&path).unwrap();
        let mirror = crate::event_store::sqlite::SqliteMirror::open(&db_path).unwrap();
        w2.set_mirror(mirror);
        let recovered = w2.recover_dangling().unwrap();
        assert_eq!(recovered, vec![t1.clone()]);
        drop(w2);

        // Direct SQLite probe: the mirror should now have the
        // recovered turn as an aborted row.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE turn_id = ?1",
                [&t1.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            exists, 1,
            "recovery through writer.append must hit the SqliteMirror"
        );
    }
}
