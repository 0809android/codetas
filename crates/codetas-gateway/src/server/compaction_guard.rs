//! Stops a thread that keeps compacting without making room.
//!
//! When the parts of a request that compaction cannot shrink (mostly tool definitions)
//! come close to the model's `auto_compact_token_limit`, Codex compacts again after every
//! tool call. Each compaction succeeds, so nothing ever ends the loop: one thread spent
//! 32M tokens this way over four hours.
//!
//! How often a thread compacts cannot tell this apart from a model with a small window,
//! which can healthily compact every minute. The shape of the requests can. A looping
//! thread never gets a second request in before compacting again, in any of 688 gaps, and
//! the request after a compaction stayed at least 76% of the one before it in all 690
//! looping compactions, against at most 51% in all 6 healthy ones.
//!
//! So a compaction counts as ineffective when the next request is at least 70% of the
//! size before compacting, or at or above the trigger when the catalog lists one, which
//! adds the case of a big shrink that is still over it. Being under the trigger does not
//! excuse a thread that compacts after every request: Codex also counts the model's output
//! and new tool results, so a 180k trigger with 148k left is still that loop. A second
//! request between compactions ends the streak, so a thread with a tight trigger that gets
//! work done is never stopped.
//!
//! Failing the compact request ends the turn with a visible error. It never touches the
//! history. One compaction per refusal gets through after about a minute, so a fix the
//! user made takes effect quickly. A real loop is refused again at its next compaction.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// A compaction achieved nothing if the next request is at least this share of the
/// largest request before it.
const INEFFECTIVE_PERCENT: u64 = 70;
const MAX_INEFFECTIVE_IN_A_ROW: u32 = 6;
/// How long after a refusal a stopped thread may try one more compaction.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// A thread's state is dropped after this long with no finished request or compaction.
const FORGET_AFTER: Duration = Duration::from_secs(10 * 60);

pub(super) const LOOP_ERROR_CODE: &str = "compaction_loop_detected";

pub(super) fn loop_error_message() -> String {
    format!(
        "CODETAS stopped this thread from compacting again: the last \
         {MAX_INEFFECTIVE_IN_A_ROW} compactions freed almost no room, which usually means the \
         model's context window is set too low. Check its context window in CODETAS, or disable \
         MCP tools you do not need, then try again in about a minute. The conversation history \
         was not changed."
    )
}

/// A finished compaction whose effect the next request will show.
struct PendingCompaction {
    size_before: u64,
    finished_at: Instant,
    /// The size at which Codex compacts this model, when the catalog lists it.
    trigger: Option<u64>,
}

impl PendingCompaction {
    fn achieved_nothing(&self, next_input_tokens: u64) -> bool {
        next_input_tokens.saturating_mul(100)
            >= self.size_before.saturating_mul(INEFFECTIVE_PERCENT)
            || self
                .trigger
                .is_some_and(|trigger| next_input_tokens >= trigger)
    }
}

struct ThreadProgress {
    largest_since_compaction: u64,
    awaiting_result: Option<PendingCompaction>,
    ineffective_in_a_row: u32,
    /// Set by a refusal, and cleared by the one compaction it lets through.
    retry_at: Option<Instant>,
    last_activity: Instant,
}

#[derive(Default)]
struct RecentProgress {
    threads: HashMap<String, ThreadProgress>,
}

impl RecentProgress {
    fn refuses_compaction(&mut self, thread: &str, now: Instant) -> bool {
        self.forget_expired(now);
        let Some(progress) = self.threads.get_mut(thread) else {
            return false;
        };
        if progress.ineffective_in_a_row < MAX_INEFFECTIVE_IN_A_ROW {
            progress.retry_at = None;
            return false;
        }
        // Each pass needs a refusal before it, however long the next request takes, so
        // a loop slower than a minute per cycle cannot walk through.
        match progress.retry_at {
            Some(retry_at) if now >= retry_at => {
                progress.retry_at = None;
                false
            }
            Some(_) => true,
            None => {
                progress.retry_at = Some(now + RETRY_AFTER);
                true
            }
        }
    }

    fn note_request(&mut self, thread: &str, input_tokens: u64, started: Instant, now: Instant) {
        // A response with no usage says nothing about the size of the thread, so it
        // neither counts towards a loop nor ends one.
        if input_tokens == 0 {
            return;
        }
        self.forget_expired(now);
        let progress = self.thread(thread, now);
        progress.last_activity = now;
        match progress.awaiting_result.take() {
            // A stream can finish just after an instant local compaction. It began
            // first, so its size says nothing about what the compaction achieved.
            Some(mut pending) if started < pending.finished_at => {
                pending.size_before = pending.size_before.max(input_tokens);
                progress.awaiting_result = Some(pending);
            }
            Some(pending) => {
                progress.ineffective_in_a_row = if pending.achieved_nothing(input_tokens) {
                    progress.ineffective_in_a_row.saturating_add(1)
                } else {
                    0
                };
                progress.largest_since_compaction = input_tokens;
            }
            // A second request with no compaction in between means the thread is
            // under its trigger again, including after its limit was corrected.
            None => {
                progress.ineffective_in_a_row = 0;
                progress.largest_since_compaction =
                    progress.largest_since_compaction.max(input_tokens);
            }
        }
    }

    fn note_compaction(&mut self, thread: &str, trigger: Option<u64>, now: Instant) {
        self.forget_expired(now);
        let progress = self.thread(thread, now);
        progress.last_activity = now;
        if progress.awaiting_result.is_none() && progress.largest_since_compaction > 0 {
            progress.awaiting_result = Some(PendingCompaction {
                size_before: progress.largest_since_compaction,
                finished_at: now,
                trigger,
            });
        }
    }

    fn thread(&mut self, thread: &str, now: Instant) -> &mut ThreadProgress {
        self.threads
            .entry(thread.to_string())
            .or_insert_with(|| ThreadProgress {
                largest_since_compaction: 0,
                awaiting_result: None,
                ineffective_in_a_row: 0,
                retry_at: None,
                last_activity: now,
            })
    }

    fn forget_expired(&mut self, now: Instant) {
        self.threads.retain(|_, progress| {
            now.saturating_duration_since(progress.last_activity) < FORGET_AFTER
        });
    }
}

fn recent() -> MutexGuard<'static, RecentProgress> {
    static RECENT: OnceLock<Mutex<RecentProgress>> = OnceLock::new();
    RECENT
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Whether to fail this compaction. Not a plain query: a stopped thread's refusal starts
/// the wait, and the compaction let through after it uses that up.
pub(super) fn refuses_compaction(thread: &str) -> bool {
    recent().refuses_compaction(thread, Instant::now())
}

/// Reports the size of the final attempt of a successful request that was not itself
/// a compaction.
pub(super) fn note_request(thread: &str, input_tokens: u64, started: Instant) {
    recent().note_request(thread, input_tokens, started, Instant::now());
}

/// Only successful compactions count. A provider outage makes Codex retry the
/// same compaction, and that must not look like a loop.
pub(super) fn note_compaction(thread: &str, trigger: Option<u64>) {
    recent().note_compaction(thread, trigger, Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(recent: &mut RecentProgress, input_tokens: u64, now: Instant) {
        recent.note_request("thread", input_tokens, now, now);
    }

    fn compact_then_request(recent: &mut RecentProgress, input_tokens: u64, now: Instant) {
        recent.note_compaction("thread", None, now);
        request(recent, input_tokens, now);
    }

    fn looping_thread(now: Instant) -> RecentProgress {
        let mut recent = RecentProgress::default();
        request(&mut recent, 140_000, now);
        for _ in 0..MAX_INEFFECTIVE_IN_A_ROW {
            assert!(!recent.refuses_compaction("thread", now));
            compact_then_request(&mut recent, 138_000, now);
        }
        recent
    }

    #[test]
    fn stops_a_thread_that_compacts_after_every_request_without_shrinking() {
        let now = Instant::now();
        let mut recent = looping_thread(now);
        assert!(recent.refuses_compaction("thread", now));
        assert!(!recent.refuses_compaction("other", now));
    }

    // The measured loop: a 115,200 trigger, 130k-170k after every compaction.
    #[test]
    fn stops_a_thread_whose_requests_stay_at_or_above_its_known_trigger() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        request(&mut recent, 170_000, now);
        for input_tokens in [130_000, 170_000, 140_000, 160_000, 115_200, 150_000] {
            assert!(!recent.refuses_compaction("thread", now));
            recent.note_compaction("thread", Some(115_200), now);
            request(&mut recent, input_tokens, now);
        }
        assert!(recent.refuses_compaction("thread", now));
    }

    // A thread that compacts after every request is the failure, even under its trigger.
    #[test]
    fn a_thread_compacting_after_every_request_stops_even_under_its_known_trigger() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        request(&mut recent, 180_000, now);
        for _ in 0..MAX_INEFFECTIVE_IN_A_ROW {
            assert!(!recent.refuses_compaction("thread", now));
            recent.note_compaction("thread", Some(180_000), now);
            request(&mut recent, 148_000, now);
        }
        assert!(recent.refuses_compaction("thread", now));
    }

    // The same sizes with work between the compactions.
    #[test]
    fn a_tight_trigger_with_a_second_request_between_compactions_is_never_stopped() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        request(&mut recent, 180_000, now);
        for _ in 0..100 {
            recent.note_compaction("thread", Some(180_000), now);
            request(&mut recent, 148_000, now);
            request(&mut recent, 165_000, now);
            assert!(!recent.refuses_compaction("thread", now));
        }
    }

    // Halving passes the 70% test, but this thread is still over its trigger.
    #[test]
    fn a_known_trigger_counts_a_large_shrink_that_stays_above_it() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        request(&mut recent, 800_000, now);
        recent.note_compaction("thread", Some(100_000), now);
        request(&mut recent, 400_000, now);
        assert_eq!(recent.threads["thread"].ineffective_in_a_row, 1);
    }

    #[test]
    fn a_response_with_no_usage_neither_counts_nor_ends_a_streak() {
        let now = Instant::now();
        let mut recent = RecentProgress::default();
        request(&mut recent, 140_000, now);
        compact_then_request(&mut recent, 138_000, now);
        assert_eq!(recent.threads["thread"].ineffective_in_a_row, 1);

        recent.note_compaction("thread", None, now);
        request(&mut recent, 0, now);
        assert_eq!(recent.threads["thread"].ineffective_in_a_row, 1);
        assert!(recent.threads["thread"].awaiting_result.is_some());

        request(&mut recent, 138_000, now);
        assert_eq!(recent.threads["thread"].ineffective_in_a_row, 2);
    }

    #[test]
    fn a_small_window_that_compacts_constantly_is_not_a_loop() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        for _ in 0..100 {
            request(&mut recent, 16_000, now);
            compact_then_request(&mut recent, 4_000, now);
            assert!(!recent.refuses_compaction("thread", now));
        }
    }

    // A 180k trigger with about 148k left after compaction: each compaction barely
    // shrinks the request, but the thread still gets work done between them.
    #[test]
    fn a_thread_short_of_room_that_still_makes_progress_is_not_a_loop() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        request(&mut recent, 180_000, now);
        for _ in 0..100 {
            compact_then_request(&mut recent, 148_000, now);
            request(&mut recent, 165_000, now);
            request(&mut recent, 180_000, now);
            assert!(!recent.refuses_compaction("thread", now));
        }
    }

    #[test]
    fn one_effective_compaction_resets_the_streak() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        request(&mut recent, 140_000, now);
        for _ in 0..MAX_INEFFECTIVE_IN_A_ROW - 1 {
            compact_then_request(&mut recent, 138_000, now);
        }
        compact_then_request(&mut recent, 20_000, now);
        compact_then_request(&mut recent, 19_000, now);
        assert_eq!(recent.threads["thread"].ineffective_in_a_row, 1);
    }

    // Once the window is corrected the thread runs under its trigger again, and its
    // next compaction, however far away, must be allowed.
    #[test]
    fn a_request_without_a_compaction_unblocks_a_corrected_thread() {
        let now = Instant::now();
        let mut recent = looping_thread(now);
        assert!(recent.refuses_compaction("thread", now));
        request(&mut recent, 150_000, now);
        assert!(!recent.refuses_compaction("thread", now));
    }

    #[test]
    fn a_request_that_began_before_a_compaction_is_not_judged_as_its_result() {
        let mut recent = RecentProgress::default();
        let start = Instant::now();
        request(&mut recent, 890_000, start);
        let compacted = start + Duration::from_secs(10);
        for _ in 0..MAX_INEFFECTIVE_IN_A_ROW {
            recent.note_compaction("thread", None, compacted);
            // The stream that triggered the compaction reports after it.
            recent.note_request("thread", 900_000, start, compacted);
            assert_eq!(recent.threads["thread"].ineffective_in_a_row, 0);
            recent.note_request("thread", 150_000, compacted, compacted);
            assert_eq!(recent.threads["thread"].ineffective_in_a_row, 0);
            recent.note_request("thread", 890_000, compacted, compacted);
        }
    }

    #[test]
    fn a_compaction_with_no_known_size_before_it_is_not_judged() {
        let mut recent = RecentProgress::default();
        let now = Instant::now();
        for _ in 0..MAX_INEFFECTIVE_IN_A_ROW {
            recent.note_compaction("thread", None, now);
        }
        request(&mut recent, 140_000, now);
        assert_eq!(recent.threads["thread"].ineffective_in_a_row, 0);
    }

    #[test]
    fn a_stopped_thread_gets_one_compaction_a_minute_to_show_a_fix_worked() {
        let start = Instant::now();
        let mut recent = looping_thread(start);
        assert!(recent.refuses_compaction("thread", start));
        assert!(recent.refuses_compaction("thread", start + RETRY_AFTER / 2));
        let retry = start + RETRY_AFTER;
        assert!(!recent.refuses_compaction("thread", retry));
        // The pass is used up, however quickly the next attempt comes.
        assert!(recent.refuses_compaction("thread", retry));
        assert!(recent.refuses_compaction("thread", retry + RETRY_AFTER / 2));
        assert!(!recent.refuses_compaction("thread", retry + RETRY_AFTER));
    }

    // A cycle in the 32M token loop took about a minute, so waiting alone must not pass.
    #[test]
    fn a_loop_slower_than_the_retry_wait_is_refused_at_its_next_compaction() {
        let mut now = Instant::now();
        let mut recent = looping_thread(now);
        assert!(recent.refuses_compaction("thread", now));
        now += RETRY_AFTER;
        assert!(!recent.refuses_compaction("thread", now));
        recent.note_compaction("thread", None, now);
        now += RETRY_AFTER * 3;
        recent.note_request("thread", 138_000, now, now);
        assert!(recent.refuses_compaction("thread", now));
    }

    #[test]
    fn a_retry_that_frees_room_clears_the_stop() {
        let mut now = Instant::now();
        let mut recent = looping_thread(now);
        assert!(recent.refuses_compaction("thread", now));
        now += RETRY_AFTER;
        assert!(!recent.refuses_compaction("thread", now));
        recent.note_compaction("thread", None, now);
        recent.note_request("thread", 20_000, now, now);
        assert!(!recent.refuses_compaction("thread", now));
    }

    #[test]
    fn a_later_stop_starts_with_a_refusal_rather_than_a_leftover_pass() {
        let start = Instant::now();
        let mut recent = looping_thread(start);
        assert!(recent.refuses_compaction("thread", start));
        // Unblocked before the pass was used, then stopped again much later.
        let later = start + RETRY_AFTER * 2;
        request(&mut recent, 150_000, later);
        for _ in 0..MAX_INEFFECTIVE_IN_A_ROW {
            assert!(!recent.refuses_compaction("thread", later));
            compact_then_request(&mut recent, 150_000, later);
        }
        assert!(recent.refuses_compaction("thread", later));
    }

    #[test]
    fn the_error_says_a_retry_will_work_soon_and_the_history_is_safe() {
        let message = loop_error_message();
        assert!(message.contains("try again in about a minute"), "{message}");
        assert!(message.contains("history was not changed"), "{message}");
    }

    #[test]
    fn a_stopped_thread_may_try_again_after_it_goes_quiet() {
        let start = Instant::now();
        let mut recent = looping_thread(start);
        assert!(recent.refuses_compaction("thread", start));
        assert!(!recent.refuses_compaction("thread", start + FORGET_AFTER));
        assert!(recent.threads.is_empty());
    }
}
