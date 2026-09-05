use super::*;

#[test]
fn log_ring_caps_at_max_entries() {
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for i in 0..(MAX_LOG_ENTRIES + 10) {
        push_log_ring(&mut ring, format!("line {i}"));
    }
    assert_eq!(ring.len(), MAX_LOG_ENTRIES);
    assert_eq!(ring.front().unwrap(), &format!("line {}", 10)); // oldest 10 dropped
}

#[test]
fn push_log_ring_returns_evicted_entry() {
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for i in 0..MAX_LOG_ENTRIES {
        assert!(push_log_ring(&mut ring, format!("line {i}")).is_empty());
    }
    // At cap: one more push evicts exactly the oldest entry, returned whole.
    let evicted = push_log_ring(&mut ring, "newest".to_string());
    assert_eq!(evicted, vec!["line 0".to_string()]);
    assert_eq!(ring.len(), MAX_LOG_ENTRIES);
}

#[test]
fn evicted_multiline_entry_is_returned_intact_for_full_line_deletion() {
    // A MessageComplete hexdump entry is ONE ring entry but spans several
    // buffer lines. When it reaches the front and is evicted it must be
    // returned whole so append_log_entry deletes all of its buffer lines
    // (not just the first) — this is the buffer/ring-desync regression.
    let mut ring: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let multiline = "Message complete\n00000000  DE AD BE EF |....|".to_string();
    assert!(push_log_ring(&mut ring, multiline.clone()).is_empty()); // front entry
    for i in 0..(MAX_LOG_ENTRIES - 1) {
        assert!(push_log_ring(&mut ring, format!("line {i}")).is_empty());
    }
    assert_eq!(ring.len(), MAX_LOG_ENTRIES);
    // One more push evicts the multi-line front entry, returned intact.
    let evicted = push_log_ring(&mut ring, "tail".to_string());
    assert_eq!(evicted, vec![multiline.clone()]);
    // It occupies 2 buffer lines — the count append_log_entry deletes.
    assert_eq!(entry_buffer_lines(&evicted[0]), 2);
}

#[test]
fn entry_buffer_lines_counts_all_lines() {
    assert_eq!(entry_buffer_lines("single"), 1);
    assert_eq!(entry_buffer_lines("a\nb\nc"), 3);
}
