use super::*;

#[tokio::test]
async fn replaced_registration_keeps_its_completion_and_successor_deadline() {
    let due = Arc::new(DueTimes::default());
    let now = sim_now();
    let key = TimerRef::State {
        tenant: "alpha".into(),
        entity_type: "Job".into(),
        entity_id: "repeating".into(),
    };
    let firing = due.arm(key.clone(), now - chrono::Duration::seconds(1));
    let mut first = due.wake_overdue("alpha", now).pop().unwrap();
    // A reset_on timeout dispatch arms this before the firing task ends.
    let next = now + chrono::Duration::hours(1);
    let successor = due.arm(key, next);
    assert!(matches!(
        first.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let mut concurrent = due.wake_overdue("alpha", now).pop().unwrap();
    assert!(matches!(
        concurrent.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert!(due.wake_overdue("beta", now).is_empty());
    due.end(&firing, true);
    assert!(first.await.unwrap());
    assert!(concurrent.await.unwrap());
    assert_eq!(
        due.answer("alpha", Waker::ControlPlane).next_due_at,
        Some(next)
    );
    assert!(due.wake_overdue("alpha", now).is_empty());
    assert!(due.lock().superseded.is_empty());
    due.end(&successor, false);
    assert_eq!(due.reportable(None), None);
}
