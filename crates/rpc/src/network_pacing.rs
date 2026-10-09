//! Client-side pacing for bulk remote uploads.
//!
//! Large remote file uploads share one connection with interactive traffic
//! (terminal input, buffer edits, LSP requests). Sending a whole file as a
//! single message blocks every other message until it has been written, so
//! uploads are split into bounded chunks by the caller and the delay between
//! chunks is chosen here.
//!
//! Two signals drive the choice:
//!
//! - [`interactive_activity_age`], updated whenever the client sends a
//!   message that is not bulk upload data or background polling. While the
//!   user is working, uploads yield a large part of the connection.
//! - The round trip of the previous chunk request, which rises when the link
//!   is congested even if the client itself is quiet.
//!
//! The delay is proportional to the observed round trip instead of a fixed
//! rate, so it adapts to the actual link without measuring bandwidth.

use std::{
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::proto;

/// The connection is treated as idle when no interactive message has been
/// sent for this long.
pub const INTERACTIVE_IDLE_GRACE: Duration = Duration::from_secs(2);

/// Activity within this window means a keystroke or edit is likely pending.
const RECENT_ACTIVITY: Duration = Duration::from_millis(400);

/// Chunk size while the connection is busy or congested.
pub const BUSY_CHUNK_SIZE: usize = 64 * 1024;

/// Chunk size while the connection is idle. Kept at a size whose transmission
/// time stays below the interactive grace period on slow links so a new
/// interaction is never stuck behind much queued data.
pub const IDLE_CHUNK_SIZE: usize = 1024 * 1024;

/// Upper bound for a single pacing delay, so a pathological round trip cannot
/// stall an upload indefinitely.
const MAX_CHUNK_DELAY: Duration = Duration::from_millis(500);

/// Round trip assumed until a chunk has completed.
const ASSUMED_ROUND_TRIP: Duration = Duration::from_millis(100);

/// A round trip this many times slower than the fastest observed one counts as
/// congestion.
const CONGESTION_FACTOR: u32 = 2;

/// A round trip this many times slower than the fastest observed one counts as
/// severe congestion.
const SEVERE_CONGESTION_FACTOR: u32 = 4;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();
static LAST_INTERACTIVE_ACTIVITY_MILLIS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Records that an interactive (non-bulk, non-polling) message was sent on a
/// remote connection. Cheap enough to call for every such message.
pub fn record_interactive_activity() {
    let start = *PROCESS_START.get_or_init(Instant::now);
    let elapsed = Instant::now().saturating_duration_since(start);
    LAST_INTERACTIVE_ACTIVITY_MILLIS.store(
        elapsed.as_millis().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}

/// Time since the last interactive remote message, or `None` when no
/// interactive message has been sent yet.
pub fn interactive_activity_age() -> Option<Duration> {
    let recorded = LAST_INTERACTIVE_ACTIVITY_MILLIS.load(Ordering::Relaxed);
    if recorded == u64::MAX {
        return None;
    }
    let start = *PROCESS_START.get_or_init(Instant::now);
    let now = Instant::now()
        .saturating_duration_since(start)
        .as_millis()
        .min(u64::MAX as u128) as u64;
    Some(Duration::from_millis(now.saturating_sub(recorded)))
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_interactive_activity() {
    LAST_INTERACTIVE_ACTIVITY_MILLIS.store(u64::MAX, Ordering::Relaxed);
}

/// Whether a payload represents interactive work rather than bulk upload data
/// or background polling. Keeping polls out matters: the activity indicator
/// polls system statistics periodically and must not keep uploads throttled.
pub fn is_interactive_payload(payload: &Option<proto::envelope::Payload>) -> bool {
    !matches!(
        payload,
        Some(
            proto::envelope::Payload::Ack(_)
                | proto::envelope::Payload::Ping(_)
                | proto::envelope::Payload::FlushBufferedMessages(_)
                | proto::envelope::Payload::GetSystemStats(_)
                | proto::envelope::Payload::GetSystemStatsResponse(_)
                | proto::envelope::Payload::BeginProjectEntryUpload(_)
                | proto::envelope::Payload::BeginProjectEntryUploadResponse(_)
                | proto::envelope::Payload::WriteProjectEntryChunk(_)
                | proto::envelope::Payload::WriteProjectEntryChunkResponse(_)
                | proto::envelope::Payload::FinishProjectEntryUpload(_)
                | proto::envelope::Payload::FinishProjectEntryUploadResponse(_)
                | proto::envelope::Payload::AbortProjectEntryUpload(_)
                | proto::envelope::Payload::AbortProjectEntryUploadResponse(_)
        )
    )
}

/// How the next upload chunk should be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadChunkPlan {
    pub chunk_size: usize,
    pub delay: Duration,
}

/// Chooses chunk sizes and inter-chunk delays for one remote upload.
#[derive(Default)]
pub struct UploadPacer {
    fastest_round_trip: Option<Duration>,
    last_round_trip: Option<Duration>,
    observed_round_trips: usize,
}

impl UploadPacer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the round trip of the chunk request that just completed.
    pub fn observe_round_trip(&mut self, round_trip: Duration) {
        self.last_round_trip = Some(round_trip);
        if self.observed_round_trips > 0 {
            self.fastest_round_trip = Some(match self.fastest_round_trip {
                Some(fastest) if fastest < round_trip => fastest,
                _ => round_trip,
            });
        }
        self.observed_round_trips += 1;
    }

    /// Plans the next chunk, given how long ago the client last sent
    /// interactive traffic.
    pub fn plan_next_chunk(&self, idle: Option<Duration>) -> UploadChunkPlan {
        let congested = self.congestion_level();
        let busy = idle.is_some_and(|idle| idle < INTERACTIVE_IDLE_GRACE);
        if !busy && congested == 0 {
            return UploadChunkPlan {
                chunk_size: IDLE_CHUNK_SIZE,
                delay: Duration::ZERO,
            };
        }

        let recently_active = idle.is_some_and(|idle| idle < RECENT_ACTIVITY);
        let divisor = if recently_active || congested >= 2 {
            3
        } else {
            1
        };
        let round_trip = self.last_round_trip.unwrap_or(ASSUMED_ROUND_TRIP);

        UploadChunkPlan {
            chunk_size: BUSY_CHUNK_SIZE,
            delay: round_trip.saturating_mul(divisor).min(MAX_CHUNK_DELAY),
        }
    }

    /// Delay to apply before starting the next entry of a directory upload.
    /// Small files are sent as whole messages, so the only way to pace them is
    /// to pause between entries.
    pub fn entry_delay(&self, idle: Option<Duration>) -> Duration {
        let plan = self.plan_next_chunk(idle);
        if plan.chunk_size == IDLE_CHUNK_SIZE {
            Duration::ZERO
        } else {
            plan.delay
        }
    }

    fn congestion_level(&self) -> u32 {
        let (Some(fastest), Some(last)) = (self.fastest_round_trip, self.last_round_trip) else {
            return 0;
        };
        if fastest.is_zero() {
            return 0;
        }
        if last >= fastest.saturating_mul(SEVERE_CONGESTION_FACTOR) {
            2
        } else if last >= fastest.saturating_mul(CONGESTION_FACTOR) {
            1
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn an_idle_connection_uses_large_chunks_without_delay() {
        let pacer = UploadPacer::new();
        let plan = pacer.plan_next_chunk(None);
        assert_eq!(plan.chunk_size, IDLE_CHUNK_SIZE);
        assert_eq!(plan.delay, Duration::ZERO);
        assert_eq!(pacer.entry_delay(Some(millis(10_000))), Duration::ZERO);
    }

    #[test]
    fn recent_activity_halves_or_quarters_the_duty_cycle() {
        let mut pacer = UploadPacer::new();
        pacer.observe_round_trip(millis(100));
        pacer.observe_round_trip(millis(100));

        let working = pacer.plan_next_chunk(Some(millis(1_000)));
        assert_eq!(working.chunk_size, BUSY_CHUNK_SIZE);
        assert_eq!(working.delay, millis(100));

        let typing = pacer.plan_next_chunk(Some(millis(50)));
        assert_eq!(typing.chunk_size, BUSY_CHUNK_SIZE);
        assert_eq!(typing.delay, millis(300));
        assert_eq!(pacer.entry_delay(Some(millis(50))), millis(300));
    }

    #[test]
    fn congestion_slows_an_otherwise_idle_upload() {
        let mut pacer = UploadPacer::new();
        pacer.observe_round_trip(millis(100));
        pacer.observe_round_trip(millis(100));
        assert_eq!(pacer.plan_next_chunk(None).chunk_size, IDLE_CHUNK_SIZE);

        pacer.observe_round_trip(millis(250));
        let congested = pacer.plan_next_chunk(None);
        assert_eq!(congested.chunk_size, BUSY_CHUNK_SIZE);
        assert_eq!(congested.delay, millis(250));

        pacer.observe_round_trip(millis(600));
        let severe = pacer.plan_next_chunk(None);
        assert_eq!(severe.chunk_size, BUSY_CHUNK_SIZE);
        assert_eq!(severe.delay, millis(500), "delay is capped");
    }

    #[test]
    fn a_single_slow_first_round_trip_does_not_report_congestion() {
        let mut pacer = UploadPacer::new();
        pacer.observe_round_trip(millis(900));
        assert_eq!(pacer.plan_next_chunk(None).chunk_size, IDLE_CHUNK_SIZE);
    }

    #[test]
    fn interactive_activity_is_tracked_but_polling_is_not() {
        // Other tests in this binary may also record activity, so only the
        // recorded direction is asserted.
        record_interactive_activity();
        let age = interactive_activity_age().expect("activity recorded");
        assert!(age < Duration::from_secs(5));

        assert!(is_interactive_payload(&Some(
            proto::envelope::Payload::SaveBuffer(proto::SaveBuffer {
                project_id: 0,
                buffer_id: 0,
                version: Vec::new(),
                new_path: None,
            })
        )));
        assert!(!is_interactive_payload(&Some(
            proto::envelope::Payload::Ping(proto::Ping {})
        )));
        assert!(!is_interactive_payload(&Some(
            proto::envelope::Payload::GetSystemStats(proto::GetSystemStats {})
        )));
        assert!(!is_interactive_payload(&Some(
            proto::envelope::Payload::WriteProjectEntryChunk(proto::WriteProjectEntryChunk {
                project_id: 0,
                worktree_id: 0,
                upload_id: 1,
                offset: 0,
                content: Vec::new(),
            })
        )));
    }
}
