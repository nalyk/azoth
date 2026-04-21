//! `clock` — the Chronon CP-4 tool. Time enters the agent's reasoning as
//! structured fact via an explicit tool invocation, never as a string
//! baked into the cache-stable constitution lane. Preserves invariant
//! #1 (transcript is not memory) and the Chronon design principle "time
//! is taint, not preface."
//!
//! The tool reads `ctx.clock` — under `SystemClock` the output reflects
//! real wall-clock; under `VirtualClock` (replay) the output reflects
//! the replay position. Same seed ⇒ byte-identical replay.

use crate::execution::{ExecutionContext, Tool, ToolError};
use crate::schemas::EffectClass;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::UNIX_EPOCH;

pub struct ClockTool;

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClockInput {
    /// Current wall-clock now — RFC3339 UTC + Unix epoch seconds.
    Now,
}

#[derive(Debug, Serialize)]
pub struct ClockOutput {
    /// RFC3339 UTC representation, suitable for display and for
    /// comparison against `observed_at`/`valid_at` fields on evidence
    /// (CP-3).
    pub iso: String,
    /// Unix epoch seconds. Stable across process restart; safe for
    /// arithmetic the model may want to do inline.
    pub epoch_secs: u64,
}

#[async_trait]
impl Tool for ClockTool {
    type Input = ClockInput;
    type Output = ClockOutput;

    fn name(&self) -> &'static str {
        "clock"
    }

    fn effect_class(&self) -> EffectClass {
        EffectClass::Observe
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["now"],
                    "description": "Clock operation. `now` returns current UTC."
                }
            },
            "required": ["op"]
        })
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ExecutionContext,
    ) -> Result<Self::Output, ToolError> {
        match input {
            ClockInput::Now => {
                // Read the clock exactly once — `iso` and `epoch_secs`
                // must describe the same instant. A second `now_iso()`
                // read would cross a wall-clock second boundary under
                // `SystemClock`, or race a concurrent `advance()` under
                // `VirtualClock`, desyncing the two fields.
                let st = ctx.clock.now();
                let epoch_secs = st
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| ToolError::Failed(format!("clock before epoch: {e}")))?
                    .as_secs();
                let iso = crate::execution::clock::rfc3339_of(st);
                Ok(ClockOutput { iso, epoch_secs })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::ArtifactStore;
    use crate::execution::{FrozenClock, VirtualClock};
    use crate::schemas::{RunId, TurnId};
    use std::sync::Arc;
    use std::time::Duration;

    fn ctx(clock: Arc<dyn crate::execution::Clock>) -> ExecutionContext {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // leak the tempdir for the duration of the test — we only read
        // ctx fields, no files touched here.
        std::mem::forget(dir);
        ExecutionContext::builder(
            RunId::from("r".to_string()),
            TurnId::from("t".to_string()),
            ArtifactStore::open(&root).unwrap(),
            root,
        )
        .clock(clock)
        .build()
    }

    #[tokio::test]
    async fn clock_tool_uses_injected_frozen_clock() {
        let clock = Arc::new(FrozenClock::from_unix_secs(1_700_000_000));
        let ctx = ctx(clock);
        let tool = ClockTool;
        let out = tool.execute(ClockInput::Now, &ctx).await.unwrap();
        assert_eq!(out.iso, "2023-11-14T22:13:20Z");
        assert_eq!(out.epoch_secs, 1_700_000_000);
    }

    #[tokio::test]
    async fn clock_tool_reads_virtual_clock_advance() {
        let clock = Arc::new(VirtualClock::from_unix_secs(1_700_000_000));
        let ctx = ctx(clock.clone());
        let tool = ClockTool;

        let first = tool.execute(ClockInput::Now, &ctx).await.unwrap();
        assert_eq!(first.epoch_secs, 1_700_000_000);

        clock.advance(Duration::from_secs(3600));

        let second = tool.execute(ClockInput::Now, &ctx).await.unwrap();
        assert_eq!(second.epoch_secs, 1_700_003_600);
        assert_ne!(first.iso, second.iso);
    }

    #[test]
    fn clock_tool_schema_conforms_to_provider_naming() {
        let t = ClockTool;
        assert_eq!(t.name(), "clock");
        assert!(matches!(t.effect_class(), EffectClass::Observe));
    }

    #[tokio::test]
    async fn clock_input_parses_from_json() {
        let v = serde_json::json!({"op": "now"});
        let parsed: ClockInput = serde_json::from_value(v).unwrap();
        assert!(matches!(parsed, ClockInput::Now));
    }

    /// Regression for codex P2 (2026-04-20): `execute` used to sample the
    /// wall clock twice — once for `epoch_secs` (via `ctx.clock.now()`)
    /// and once for `iso` (via `ctx.clock.now_iso()`). Under `SystemClock`
    /// the two reads can straddle a second boundary; under `VirtualClock`
    /// a concurrent `advance()` can race between them. Either way the two
    /// output fields describe different instants. The fix collapses to a
    /// single `now()` sample shared by both fields.
    #[tokio::test]
    async fn clock_tool_reads_wall_clock_exactly_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{Instant, SystemTime};

        #[derive(Debug)]
        struct RacingClock {
            reads: AtomicU32,
        }
        impl crate::execution::Clock for RacingClock {
            fn now(&self) -> SystemTime {
                let n = self.reads.fetch_add(1, Ordering::SeqCst);
                // Each successive call returns a strictly later wall time.
                // A pre-fix implementation that called the clock twice
                // would yield `iso` one second ahead of `epoch_secs`.
                std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n as u64)
            }
            fn now_instant(&self) -> Instant {
                Instant::now()
            }
        }

        let racing = Arc::new(RacingClock {
            reads: AtomicU32::new(0),
        });
        let clock: Arc<dyn crate::execution::Clock> = racing.clone();
        let ctx = ctx(clock);
        let tool = ClockTool;
        let out = tool.execute(ClockInput::Now, &ctx).await.unwrap();

        assert_eq!(
            racing.reads.load(Ordering::SeqCst),
            1,
            "ClockTool must read wall-clock exactly once per execute"
        );
        assert_eq!(out.epoch_secs, 1_700_000_000);
        assert_eq!(out.iso, "2023-11-14T22:13:20Z");
    }
}
