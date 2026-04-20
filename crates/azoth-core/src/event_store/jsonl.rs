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
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
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

    /// Open an existing session file for resume. Errors with `NotFound` if
    /// the file does not exist. Runs `recover_dangling_turns` exactly once
    /// before handing back a writer positioned at EOF — idempotent on a
    /// fully-recovered file.
    pub fn open_existing<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("session file not found: {}", path.display()),
            ));
        }
        JsonlReader::open(path)
            .recover_dangling_turns()
            .map_err(io::Error::other)?;
        Self::open(path)
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
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?;
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
    fn scan(&self) -> Result<Scan, ProjectionError> {
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
    fn scan_as_of(&self, as_of: &str) -> Result<Scan, ProjectionError> {
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
        // ≤ as_of.
        let mut visible: std::collections::HashSet<TurnId> = std::collections::HashSet::new();
        for (turn_id, maybe_at) in &terminal_at {
            let effective = maybe_at
                .as_deref()
                .or_else(|| turn_started_at.get(turn_id).map(String::as_str));
            if let Some(ts) = effective {
                if ts <= as_of {
                    visible.insert(turn_id.clone());
                }
            }
        }

        // Second pass: filter.
        let events: Vec<SessionEvent> = raw
            .events
            .into_iter()
            .filter(|ev| match ev {
                SessionEvent::RunStarted { timestamp, .. }
                | SessionEvent::ContractAccepted { timestamp, .. } => timestamp.as_str() <= as_of,
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
    /// Timestamps compare lexicographically, which is equivalent to
    /// chronological comparison for RFC3339 strings with a fixed `Z`
    /// offset — the format every `Clock` impl in `execution::clock`
    /// produces.
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
        for ev in events {
            match ev {
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
        let scan = self.scan()?;
        // Index the last seen heartbeat per turn. Walk events forward so
        // the final assignment is the most recent heartbeat — same shape
        // as the outcomes map.
        let mut last_heartbeat: HashMap<TurnId, String> = HashMap::new();
        for ev in &scan.events {
            if let SessionEvent::TurnHeartbeat { turn_id, at, .. } = ev {
                if !at.is_empty() {
                    last_heartbeat.insert(turn_id.clone(), at.clone());
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
        if dangling.is_empty() {
            return Ok(dangling);
        }

        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        // Ensure we start on a fresh line.
        file.seek(SeekFrom::End(0))?;
        for turn_id in &dangling {
            let synthetic = match last_heartbeat.get(turn_id) {
                Some(hb_at) => {
                    // Heartbeat evidence exists → reclassify as Stalled.
                    // The `at` field carries the last heartbeat, not
                    // "now" — operators need to see when progress
                    // actually stopped, not when resume noticed.
                    SessionEvent::TurnAborted {
                        turn_id: turn_id.clone(),
                        reason: AbortReason::Stalled,
                        detail: Some(format!("last heartbeat at {hb_at}")),
                        usage: Usage::default(),
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
            let line = serialize_line(&synthetic)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.sync_data()?;
        Ok(dangling)
    }
}

struct Scan {
    events: Vec<SessionEvent>,
    outcomes: HashMap<TurnId, TurnOutcomeKind>,
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
    fn open_existing_runs_recovery_once_then_idempotent() {
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

        // First open_existing: recovery appends exactly one TurnInterrupted.
        let w1 = JsonlWriter::open_existing(&path).unwrap();
        drop(w1);
        let count_after_first = JsonlReader::open(&path)
            .forensic()
            .unwrap()
            .iter()
            .filter(|f| matches!(&f.event, SessionEvent::TurnInterrupted { .. }))
            .count();
        assert_eq!(count_after_first, 1);

        // Second open_existing on the recovered file: still exactly one.
        let w2 = JsonlWriter::open_existing(&path).unwrap();
        drop(w2);
        let count_after_second = JsonlReader::open(&path)
            .forensic()
            .unwrap()
            .iter()
            .filter(|f| matches!(&f.event, SessionEvent::TurnInterrupted { .. }))
            .count();
        assert_eq!(count_after_second, 1);
    }
}
