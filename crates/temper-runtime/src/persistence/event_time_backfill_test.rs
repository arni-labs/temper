use super::*;

fn times() -> JournalTimes {
    JournalTimes {
        first_missing: true,
        last_missing: true,
        state_is_object: true,
        has_events: true,
        creation_at: Some("2026-10-05T09:00:00.5+02:00".into()),
        newest_sequence_nr: Some(3),
        newest_at: Some("2026-10-06 08:00:00".into()),
        snapshot_sequence_nr: None,
    }
}

#[test]
fn a_record_with_its_journal_is_filled_from_creation_and_newest_event() {
    let (first, last) = times().plan();
    assert_eq!(
        first,
        FirstFill::Fill(journal_time("2026-10-05T07:00:00.5Z").unwrap())
    );
    assert_eq!(
        last,
        LastFill::Fill(journal_time("2026-10-06T08:00:00Z").unwrap())
    );
    let FirstFill::Fill(at) = first else {
        unreachable!()
    };
    assert_eq!(first_event_at_text(at), "2026-10-05T07:00:00.500000000Z");
}

#[test]
fn a_set_time_a_lost_creation_a_snapshot_and_no_journal_are_left() {
    let present = JournalTimes {
        first_missing: false,
        last_missing: false,
        ..times()
    };
    assert_eq!(present.plan(), (FirstFill::Present, LastFill::Present));
    let lost_creation = JournalTimes {
        creation_at: None,
        ..times()
    };
    assert_eq!(lost_creation.plan().0, FirstFill::CreationNotHeld);
    let snapshotted = JournalTimes {
        snapshot_sequence_nr: Some(3),
        ..times()
    };
    assert_eq!(snapshotted.plan().1, LastFill::HeldBySnapshot);
    let tail_after_snapshot = JournalTimes {
        snapshot_sequence_nr: Some(2),
        ..times()
    };
    assert!(matches!(tail_after_snapshot.plan().1, LastFill::Fill(_)));
    let none = JournalTimes {
        has_events: false,
        creation_at: None,
        newest_sequence_nr: None,
        newest_at: None,
        ..times()
    };
    assert_eq!(none.plan(), (FirstFill::Unfillable, LastFill::Unfillable));
    let garbled = JournalTimes {
        creation_at: Some("yesterday".into()),
        newest_at: Some("yesterday".into()),
        ..times()
    };
    assert_eq!(
        garbled.plan(),
        (FirstFill::Unfillable, LastFill::Unfillable)
    );
}

#[test]
fn only_fill_switches_the_backfill_to_writing() {
    assert_eq!(
        EventTimeBackfillMode::parse(None),
        EventTimeBackfillMode::Count
    );
    assert_eq!(
        EventTimeBackfillMode::parse(Some(" Fill ")),
        EventTimeBackfillMode::Fill
    );
    assert_eq!(
        EventTimeBackfillMode::parse(Some("yes")),
        EventTimeBackfillMode::Count
    );
}

#[test]
fn the_tally_adds_up_by_tenant_and_type() {
    let mut tally = EventTimeTally::default();
    let (first, last) = times().plan();
    tally.record("b", "Ask", first, last);
    tally.record("a", "Ask", FirstFill::Present, LastFill::HeldBySnapshot);
    tally.record("a", "Ask", FirstFill::Present, LastFill::Present);
    let gaps = tally.into_gaps();
    assert_eq!(gaps.len(), 2);
    assert_eq!(gaps[0].tenant, "a");
    assert_eq!(gaps[0].records, 2);
    assert_eq!(gaps[0].missing_last, 1);
    assert_eq!(gaps[0].last_held_by_snapshot, 1);
    assert_eq!(gaps[1].first_fillable, 1);
    assert_eq!(gaps[1].last_fillable, 1);
}
