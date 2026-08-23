use super::*;

/// Data-socket stand-in that times out `stalls_remaining` times
/// before accepting bytes, like a peer whose receive window is
/// closed.
struct StallingWriter {
    stalls_remaining: u32,
    written: Vec<u8>,
}

impl Write for StallingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.stalls_remaining > 0 {
            self.stalls_remaining -= 1;
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
        }
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A brief stall drops queued chunks, not the client: the chunk in
/// progress is retried, the chunks that queued up behind it are
/// discarded (counted as drops), and the slot stays live.
#[test]
fn tcp_writer_drops_queued_chunks_before_dropping_a_stalled_client() {
    /// How long the test waits for the writer to drain the stall.
    const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
    let registry = Arc::new(ClientRegistry::new());
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    for _ in 0..3 {
        slot.tx.send(STALL_TEST_CHUNK.to_vec()).expect("queue");
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let written = Arc::new(Mutex::new(Vec::new()));
    let writer_thread = {
        let (slot, registry, shutdown, written) = (
            slot.clone(),
            registry.clone(),
            shutdown.clone(),
            written.clone(),
        );
        thread::spawn(move || {
            let mut w = StallingWriter {
                stalls_remaining: 2,
                written: Vec::new(),
            };
            tcp_writer(&mut w, rx, slot, registry, shutdown, true);
            *written.lock().expect("written") = w.written;
        })
    };
    // The slot's own sender keeps the channel open, so the writer
    // idles after the stall; stop it once the drops are visible.
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while registry.total_buffers_dropped() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    shutdown.store(true, Ordering::SeqCst);
    writer_thread.join().expect("writer thread");
    assert!(
        !slot.is_disconnected(),
        "a brief stall must not kick the client"
    );
    assert_eq!(*written.lock().expect("written"), STALL_TEST_CHUNK.to_vec());
    let dropped = slot.stats.lock().expect("stats").buffers_dropped;
    assert_eq!(
        dropped, 2,
        "the two chunks queued behind the stall were dropped"
    );
    assert_eq!(registry.total_buffers_dropped(), 2);
}

/// A stall that outlasts the budget does drop the client.
#[test]
fn tcp_writer_gives_up_after_the_stall_budget() {
    let registry = Arc::new(ClientRegistry::new());
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    slot.tx.send(STALL_TEST_CHUNK.to_vec()).expect("queue");
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut w = StallingWriter {
        stalls_remaining: MAX_CONSECUTIVE_WRITE_STALLS + 1,
        written: Vec::new(),
    };
    tcp_writer(&mut w, rx, slot.clone(), registry, shutdown, true);
    assert!(slot.is_disconnected());
    assert!(w.written.is_empty());
}

/// A compressed stream cannot resume mid-block: its first stall
/// closes the client instead of being retried.
#[test]
fn tcp_writer_closes_a_compressed_stream_on_its_first_stall() {
    let registry = Arc::new(ClientRegistry::new());
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    slot.tx.send(STALL_TEST_CHUNK.to_vec()).expect("queue");
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut w = StallingWriter {
        stalls_remaining: 1,
        written: Vec::new(),
    };
    tcp_writer(&mut w, rx, slot.clone(), registry, shutdown, false);
    assert!(slot.is_disconnected());
    assert!(w.written.is_empty());
}

/// Socket stand-in that alternates one byte of progress with a
/// stall: never two stalls in a row, so the consecutive budget
/// must never trip no matter how long the chunk is.
struct TricklingWriter {
    stall_next: bool,
    written: Vec<u8>,
}

impl Write for TricklingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stall_next = !self.stall_next;
        if !self.stall_next {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
        }
        self.written.push(buf[0]);
        Ok(1)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The stall budget counts consecutive stalls: progress resets it.
#[test]
fn stall_budget_resets_on_progress() {
    let registry = ClientRegistry::new();
    let (slot, rx) = ClientSlot::new(
        registry.allocate_id(),
        test_peer(STALL_TEST_PORT),
        Codec::None,
        Role::Control,
        TEST_CLIENT_CHANNEL_DEPTH,
    );
    // Longer than the budget, so an accumulating counter would
    // close the client part-way through.
    let chunk = vec![0xA5_u8; (MAX_CONSECUTIVE_WRITE_STALLS as usize) * 3];
    let mut w = TricklingWriter {
        stall_next: false,
        written: Vec::new(),
    };
    let outcome = write_chunk_shedding_backlog(&mut w, &chunk, &rx, &slot, &registry, true);
    assert_eq!(outcome, ChunkOutcome::Sent);
    assert_eq!(w.written, chunk);
    assert!(!slot.is_disconnected());
}
