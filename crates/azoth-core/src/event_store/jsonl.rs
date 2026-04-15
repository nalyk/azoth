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
use crate::schemas::{AbortReason, SessionEvent, TurnId};
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
pub struct JsonlWriter {
    path: PathBuf,
    file: BufWriter<File>,
    tap: Option<UnboundedSender<SessionEvent>>,
    mirror: Option<SqliteMirror>,
}

impl JsonlWriter {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
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
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
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
            let ev: SessionEvent = serde_json::from_str(&line)
                .map_err(|source| ProjectionError::Parse { line: idx + 1, source })?;

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

    /// Forensic projection: every line, tagged with its replayability.
    pub fn forensic(&self) -> Result<Vec<ForensicEvent>, ProjectionError> {
        let scan = self.scan()?;
        Ok(scan
            .events
            .into_iter()
            .map(|ev| {
                let non_replayable = !is_replayable(&ev, &scan.outcomes);
                ForensicEvent { event: ev, non_replayable }
            })
            .collect())
    }

    /// Crash recovery: scan for turns with no terminal marker and append a
    /// synthetic `turn_interrupted { reason: "crash" }` record for each. Idempotent.
    pub fn recover_dangling_turns(&self) -> Result<Vec<TurnId>, ProjectionError> {
        let scan = self.scan()?;
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
            let synthetic = SessionEvent::TurnInterrupted {
                turn_id: turn_id.clone(),
                reason: AbortReason::Crash,
                partial_usage: Default::default(),
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
            block: ContentBlock::Text { text: "hello".into() },
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
                name: "repo.search".into(),
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
        assert_eq!(replay.len(), 5, "replay: {:#?}", replay.iter().map(|e| &e.0).collect::<Vec<_>>());
        for e in &replay {
            match e.0.turn_id() {
                None => {}
                Some(tid) => assert_eq!(tid, &t1),
            }
        }

        let forensic = r.forensic().unwrap();
        let non_repl = forensic.iter().filter(|f| f.non_replayable).count();
        assert!(non_repl >= 4, "expected non_replayable tags on t2+t3, got {non_repl}");
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
            SessionEvent::TurnInterrupted { reason: AbortReason::Crash, .. }
        )));
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
