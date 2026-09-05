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
