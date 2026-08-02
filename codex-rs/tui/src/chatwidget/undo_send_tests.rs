use super::*;

#[test]
fn countdown_uses_ceiling_and_never_displays_zero() {
    assert_eq!(countdown_seconds(Duration::from_secs(5)), 5);
    assert_eq!(countdown_seconds(Duration::from_millis(4001)), 5);
    assert_eq!(countdown_seconds(Duration::from_secs(4)), 4);
    assert_eq!(countdown_seconds(Duration::from_millis(1)), 1);
    assert_eq!(countdown_seconds(Duration::ZERO), 1);
}

#[test]
fn next_tick_lands_on_the_next_visible_second() {
    let now = Instant::now();
    assert_eq!(
        delay_until_next_countdown_change(now + Duration::from_secs(5), now),
        Duration::from_secs(1)
    );
    assert_eq!(
        delay_until_next_countdown_change(now + Duration::from_millis(4250), now),
        Duration::from_millis(250)
    );
}
