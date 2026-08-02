//! Truthful smoothing for cumulative turn-token snapshots.
//!
//! Token usage reaches the TUI in response-level batches rather than as a
//! per-token stream. Transitions interpolate between received snapshots and
//! sequence input before output, matching the actual request/response order
//! without implying that both directions are active simultaneously.

use std::time::Duration;
use std::time::Instant;

use crate::token_usage::TokenUsage;

const ANIMATION_FRAME_DURATION: Duration = Duration::from_millis(32);
const MIN_TRANSITION_FRAMES: u32 = 20;
const MAX_TRANSITION_FRAMES: u32 = 50;
const ACTIVITY_HOLD_DURATION: Duration = Duration::from_millis(900);
const ARROW_PULSE_DURATION: Duration = Duration::from_millis(180);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnTokenUsage {
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) total_tokens: i64,
}

impl TurnTokenUsage {
    pub(crate) fn is_empty(self) -> bool {
        self.input_tokens <= 0 && self.output_tokens <= 0 && self.total_tokens <= 0
    }

    pub(crate) fn saturating_sub(self, baseline: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(baseline.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(baseline.output_tokens),
            total_tokens: self.total_tokens.saturating_sub(baseline.total_tokens),
        }
    }

    pub(crate) fn precedes(self, other: Self) -> bool {
        self.input_tokens < other.input_tokens
            || self.output_tokens < other.output_tokens
            || self.total_tokens < other.total_tokens
    }
}

impl From<&TokenUsage> for TurnTokenUsage {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens.max(0),
            output_tokens: usage.output_tokens.max(0),
            total_tokens: usage.total_tokens.max(0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TokenUsageTransition {
    from: TurnTokenUsage,
    target: TurnTokenUsage,
    started_at: Instant,
    input_changed: bool,
    output_changed: bool,
    input_duration: Duration,
    output_duration: Duration,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TokenUsageFrame {
    pub(super) usage: TurnTokenUsage,
    pub(super) input_activity: TokenActivity,
    pub(super) output_activity: TokenActivity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TokenActivity {
    Idle,
    Moving,
    Holding,
}

impl TokenUsageTransition {
    pub(super) fn new(
        from: TurnTokenUsage,
        target: TurnTokenUsage,
        started_at: Instant,
    ) -> Option<Self> {
        let input_changed = target.input_tokens > from.input_tokens;
        let output_changed = target.output_tokens > from.output_tokens;
        (input_changed || output_changed).then_some(Self {
            from,
            target,
            started_at,
            input_changed,
            output_changed,
            input_duration: transition_duration(from.input_tokens, target.input_tokens),
            output_duration: transition_duration(from.output_tokens, target.output_tokens),
        })
    }

    pub(super) fn frame_at(self, now: Instant) -> TokenUsageFrame {
        let elapsed = now.saturating_duration_since(self.started_at);
        let input_phase_duration = if self.input_changed {
            self.input_duration + ACTIVITY_HOLD_DURATION
        } else {
            Duration::ZERO
        };
        let output_delay = if self.input_changed && self.output_changed {
            input_phase_duration
        } else {
            Duration::ZERO
        };
        let input_tokens = if self.input_changed {
            interpolate_token_count(
                self.from.input_tokens,
                self.target.input_tokens,
                elapsed,
                /*delay*/ Duration::ZERO,
                self.input_duration,
            )
        } else {
            self.target.input_tokens
        };
        let output_tokens = if self.output_changed {
            interpolate_token_count(
                self.from.output_tokens,
                self.target.output_tokens,
                elapsed,
                output_delay,
                self.output_duration,
            )
        } else {
            self.target.output_tokens
        };
        let input_activity = token_activity(
            self.input_changed,
            elapsed,
            /*delay*/ Duration::ZERO,
            self.input_duration,
        );
        let output_activity = token_activity(
            self.output_changed,
            elapsed,
            output_delay,
            self.output_duration,
        );
        let output_phase_duration = if self.output_changed {
            self.output_duration + ACTIVITY_HOLD_DURATION
        } else {
            Duration::ZERO
        };
        let total_duration = if self.output_changed {
            output_delay + output_phase_duration
        } else {
            input_phase_duration
        };
        let finished = elapsed >= total_duration;
        let residual_from = self
            .from
            .total_tokens
            .saturating_sub(self.from.input_tokens)
            .saturating_sub(self.from.output_tokens);
        let residual_target = self
            .target
            .total_tokens
            .saturating_sub(self.target.input_tokens)
            .saturating_sub(self.target.output_tokens);
        let residual = if finished {
            residual_target
        } else {
            interpolate_token_count(
                residual_from,
                residual_target,
                elapsed,
                /*delay*/ Duration::ZERO,
                total_duration,
            )
        };
        let usage = if finished {
            self.target
        } else {
            TurnTokenUsage {
                input_tokens,
                output_tokens,
                total_tokens: input_tokens
                    .saturating_add(output_tokens)
                    .saturating_add(residual),
            }
        };
        TokenUsageFrame {
            usage,
            input_activity,
            output_activity,
        }
    }

    pub(super) fn alternate_arrow_at(self, now: Instant) -> bool {
        (now.saturating_duration_since(self.started_at).as_millis()
            / ARROW_PULSE_DURATION.as_millis())
        .is_multiple_of(2)
    }
}

fn token_activity(
    changed: bool,
    elapsed: Duration,
    delay: Duration,
    duration: Duration,
) -> TokenActivity {
    if !changed || elapsed < delay {
        return TokenActivity::Idle;
    }
    let phase_elapsed = elapsed.saturating_sub(delay);
    if phase_elapsed < duration {
        TokenActivity::Moving
    } else if phase_elapsed < duration + ACTIVITY_HOLD_DURATION {
        TokenActivity::Holding
    } else {
        TokenActivity::Idle
    }
}

fn transition_duration(from: i64, target: i64) -> Duration {
    let delta = target.saturating_sub(from);
    if delta <= 0 {
        return Duration::ZERO;
    }
    let display_quantum = compact_display_quantum(from.max(target));
    let visible_steps = u64::try_from(delta)
        .unwrap_or(u64::MAX)
        .div_ceil(display_quantum);
    let frames = visible_steps.clamp(
        u64::from(MIN_TRANSITION_FRAMES),
        u64::from(MAX_TRANSITION_FRAMES),
    );
    ANIMATION_FRAME_DURATION * u32::try_from(frames).unwrap_or(MAX_TRANSITION_FRAMES)
}

fn compact_display_quantum(value: i64) -> u64 {
    match value.max(0) {
        0..=999 => 1,
        1_000..=9_999 => 10,
        10_000..=99_999 => 100,
        100_000..=999_999 => 1_000,
        1_000_000..=9_999_999 => 10_000,
        10_000_000..=99_999_999 => 100_000,
        100_000_000..=999_999_999 => 1_000_000,
        1_000_000_000..=9_999_999_999 => 10_000_000,
        10_000_000_000..=99_999_999_999 => 100_000_000,
        100_000_000_000..=999_999_999_999 => 1_000_000_000,
        1_000_000_000_000..=9_999_999_999_999 => 10_000_000_000,
        10_000_000_000_000..=99_999_999_999_999 => 100_000_000_000,
        _ => 1_000_000_000_000,
    }
}

fn interpolate_token_count(
    from: i64,
    target: i64,
    elapsed: Duration,
    delay: Duration,
    duration: Duration,
) -> i64 {
    if target <= from || elapsed <= delay {
        return from;
    }
    let phase_elapsed = elapsed.saturating_sub(delay);
    if phase_elapsed >= duration {
        return target;
    }
    let progress = phase_elapsed.as_secs_f64() / duration.as_secs_f64();
    let delta = target.saturating_sub(from) as f64;
    from.saturating_add((delta * progress).round() as i64)
}

#[cfg(test)]
#[path = "token_transition_tests.rs"]
mod tests;
