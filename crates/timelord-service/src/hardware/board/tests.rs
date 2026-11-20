use super::*;

#[tokio::test]
async fn initialization_wait_cancels_without_waiting_for_another_native_owner() {
    let _held = INITIALIZATION.lock().await;
    let (stop, mut shutdown) = watch::channel(false);
    let wait = acquire_initialization(&mut shutdown);
    tokio::pin!(wait);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut wait)
            .await
            .is_err()
    );
    stop.send(true).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .unwrap()
            .is_none()
    );
    let mut already_stopped = stop.subscribe();
    assert!(acquire_initialization(&mut already_stopped).await.is_none());
}

#[test]
fn restart_delays_follow_the_persisted_failure_streak_and_never_exhaust() {
    let timing = Timing::default();
    let mut health = crate::hardware::health::Health::default();
    let mut delays = vec![];
    for t in 0..6 {
        health.record_failure(t, "x");
        delays.push(timing.restart_delay(&health).as_secs());
    }
    assert_eq!(delays, [1, 5, 30, 30, 30, 30]);
}
