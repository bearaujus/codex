use super::*;
use pretty_assertions::assert_eq;

#[test]
fn input_and_output_snapshots_animate_in_protocol_order() {
    let started_at = Instant::now();
    let transition = TokenUsageTransition::new(
        TurnTokenUsage {
            input_tokens: 3_840_000,
            output_tokens: 9_770,
            total_tokens: 3_849_770,
        },
        TurnTokenUsage {
            input_tokens: 3_900_000,
            output_tokens: 10_000,
            total_tokens: 3_910_000,
        },
        started_at,
    )
    .expect("expected token transition");

    let input_frame = transition.frame_at(started_at + transition.input_duration / 2);
    assert_eq!(input_frame.input_activity, TokenActivity::Moving);
    assert_eq!(input_frame.output_activity, TokenActivity::Idle);
    assert!(input_frame.usage.input_tokens > 3_840_000);
    assert!(input_frame.usage.input_tokens < 3_900_000);
    assert_eq!(input_frame.usage.output_tokens, 9_770);

    let input_hold =
        transition.frame_at(started_at + transition.input_duration + ACTIVITY_HOLD_DURATION / 2);
    assert_eq!(input_hold.input_activity, TokenActivity::Holding);
    assert_eq!(input_hold.output_activity, TokenActivity::Idle);
    assert_eq!(input_hold.usage.input_tokens, 3_900_000);
    assert_eq!(input_hold.usage.output_tokens, 9_770);

    let output_started_at = started_at + transition.input_duration + ACTIVITY_HOLD_DURATION;
    let output_frame = transition.frame_at(output_started_at + transition.output_duration / 2);
    assert_eq!(output_frame.input_activity, TokenActivity::Idle);
    assert_eq!(output_frame.output_activity, TokenActivity::Moving);
    assert_eq!(output_frame.usage.input_tokens, 3_900_000);
    assert!(output_frame.usage.output_tokens > 9_770);
    assert!(output_frame.usage.output_tokens < 10_000);

    let output_hold = transition
        .frame_at(output_started_at + transition.output_duration + Duration::from_millis(1));
    assert_eq!(output_hold.output_activity, TokenActivity::Holding);
    assert_eq!(output_hold.usage.output_tokens, 10_000);

    let settled = transition
        .frame_at(output_started_at + transition.output_duration + ACTIVITY_HOLD_DURATION);
    assert_eq!(settled.input_activity, TokenActivity::Idle);
    assert_eq!(settled.output_activity, TokenActivity::Idle);
    assert_eq!(settled.usage, transition.target);
}

#[test]
fn transition_duration_tracks_visible_compact_number_steps_with_bounds() {
    assert_eq!(
        transition_duration(3_840_000, 3_900_000),
        ANIMATION_FRAME_DURATION * MIN_TRANSITION_FRAMES
    );
    assert_eq!(
        transition_duration(0, i64::MAX),
        ANIMATION_FRAME_DURATION * MAX_TRANSITION_FRAMES
    );
}
