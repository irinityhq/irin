use super::build_smoke_seat_events;

#[test]
fn emits_three_chunks_per_seat_before_seat_complete() {
    let seats = vec![
        (
            "Hawk".to_string(),
            "openrouter".to_string(),
            "m-a".to_string(),
        ),
        ("Owl".to_string(), "nous".to_string(), "m-b".to_string()),
    ];
    let events = build_smoke_seat_events("smoke-session", 1, &seats, "grok", "grok-4.3", "T");

    // Exactly one round.
    let round_starts = events
        .iter()
        .filter(|e| e.event_type == "round_started")
        .count();
    assert_eq!(round_starts, 1);

    // Per seat: 3 seat_chunk frames; total 6 across 2 seats.
    let chunks: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "seat_chunk")
        .collect();
    assert_eq!(chunks.len(), 6);

    // For each seat, the three chunks precede the seat_complete and carry
    // monotonic seq 0,1,2.
    for seat in ["Hawk", "Owl"] {
        let seat_chunk_idxs: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.event_type == "seat_chunk" && e.data["seat_name"] == seat)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(seat_chunk_idxs.len(), 3, "seat {seat} chunk count");
        let complete_idx = events
            .iter()
            .position(|e| e.event_type == "seat_complete" && e.data["seat_name"] == seat)
            .expect("seat_complete present");
        for (expected_seq, idx) in seat_chunk_idxs.iter().enumerate() {
            assert!(*idx < complete_idx, "chunk must precede seat_complete");
            assert_eq!(events[*idx].data["seq"], expected_seq as u64);
        }
        // seat_complete.text is the authoritative full text (chunk concat).
        assert_eq!(
            events[complete_idx].data["text"],
            "[smoke] synthetic stream"
        );
    }

    // Terminal ordering: synthesis_complete → session_saved → done.
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(types.contains(&"synthesis_complete"));
    assert_eq!(types.last(), Some(&"done"));
}

#[test]
fn empty_seats_still_emits_round_and_done() {
    let events = build_smoke_seat_events("smoke-session", 2, &[], "grok", "grok-4.3", "T");
    let round_starts = events
        .iter()
        .filter(|e| e.event_type == "round_started")
        .count();
    assert_eq!(round_starts, 2);
    assert!(events.iter().all(|e| e.event_type != "seat_chunk"));
    assert_eq!(events.last().map(|e| e.event_type.as_str()), Some("done"));
}
