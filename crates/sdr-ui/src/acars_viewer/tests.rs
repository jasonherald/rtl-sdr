use std::time::{Duration, SystemTime};

use super::*;
use arrayvec::ArrayString;
use sdr_acars::AcarsMessage;

fn fixture_message(tail: &str, label: [u8; 2], ts: SystemTime) -> AcarsMessage {
    let mut aircraft = ArrayString::<8>::new();
    aircraft.push_str(tail);
    AcarsMessage {
        timestamp: ts,
        channel_idx: 0,
        freq_hz: 131_550_000.0,
        level_db: 0.0,
        error_count: 0,
        mode: b'2',
        label,
        block_id: b'5',
        ack: b'!',
        aircraft,
        flight_id: None,
        message_no: None,
        text: String::new(),
        end_of_message: true,
        reassembled_block_count: 1,
        parsed: None,
    }
}

#[test]
fn aircraft_entry_object_record_message_bumps_count() {
    // GTK glib::Object subclasses can be constructed without
    // a running GTK Application — `glib::Object::new` works
    // as long as the type was registered (which happens via
    // the `#[glib::object_subclass]` macro at module load).
    gtk4::glib::MainContext::default();
    let ts = SystemTime::now();
    let entry = AircraftEntry {
        tail: {
            let mut s = ArrayString::<8>::new();
            s.push_str(".N12345");
            s
        },
        last_seen: ts,
        msg_count: 0,
        last_label: *b"H1",
    };
    let obj = AircraftEntryObject::new(entry);
    assert_eq!(obj.entry().unwrap().msg_count, 0);

    let msg = fixture_message(".N12345", *b"M1", ts + Duration::from_secs(1));
    obj.record_message(&msg);
    assert_eq!(obj.entry().unwrap().msg_count, 1);

    obj.record_message(&msg);
    assert_eq!(obj.entry().unwrap().msg_count, 2);
}

#[test]
fn aircraft_entry_object_last_seen_monotonic() {
    gtk4::glib::MainContext::default();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let entry = AircraftEntry {
        tail: ArrayString::new(),
        last_seen: t0,
        msg_count: 0,
        last_label: *b"  ",
    };
    let obj = AircraftEntryObject::new(entry);

    // Out-of-order timestamps must not regress last_seen.
    let later = fixture_message("X", *b"H1", t0 + Duration::from_mins(1));
    let earlier = fixture_message("X", *b"H1", t0 + Duration::from_secs(30));
    obj.record_message(&later);
    obj.record_message(&earlier);
    assert_eq!(obj.entry().unwrap().last_seen, t0 + Duration::from_mins(1));
}

#[test]
fn aircraft_entry_object_record_message_updates_label() {
    gtk4::glib::MainContext::default();
    let ts = SystemTime::now();
    let entry = AircraftEntry {
        tail: ArrayString::new(),
        last_seen: ts,
        msg_count: 0,
        last_label: *b"H1",
    };
    let obj = AircraftEntryObject::new(entry);
    let msg = fixture_message("X", *b"M1", ts);
    obj.record_message(&msg);
    assert_eq!(obj.entry().unwrap().last_label, *b"M1");
}
