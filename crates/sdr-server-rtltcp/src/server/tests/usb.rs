use super::*;

/// #808: any successful read — even a zero-length one — resets
/// the consecutive-error budget; a timeout leaves it untouched.
#[test]
fn zero_length_usb_read_resets_the_error_budget() {
    let mut errors = 0_u32;
    for _ in 1..MAX_CONSECUTIVE_USB_ERRORS {
        assert_eq!(
            classify_usb_read(Err(rusb::Error::Io), &mut errors),
            UsbReadOutcome::Retry(rusb::Error::Io)
        );
    }
    assert_eq!(
        classify_usb_read(Err(rusb::Error::Timeout), &mut errors),
        UsbReadOutcome::Idle
    );
    assert_eq!(errors, MAX_CONSECUTIVE_USB_ERRORS - 1, "timeout is neutral");
    assert_eq!(classify_usb_read(Ok(0), &mut errors), UsbReadOutcome::Idle);
    assert_eq!(errors, 0, "Ok(0) is a successful read");
    assert_eq!(
        classify_usb_read(Err(rusb::Error::Io), &mut errors),
        UsbReadOutcome::Retry(rusb::Error::Io)
    );
    assert_eq!(
        classify_usb_read(Ok(7), &mut errors),
        UsbReadOutcome::Data(7)
    );
    assert_eq!(errors, 0);
}

/// USB read failures are tolerated up to librtlsdr's consecutive
/// transfer-error budget; a device loss stops immediately.
#[test]
fn usb_read_failures_stop_only_after_the_consecutive_budget() {
    let mut errors = 0_u32;
    for _ in 1..MAX_CONSECUTIVE_USB_ERRORS {
        assert_eq!(
            classify_usb_read(Err(rusb::Error::Overflow), &mut errors),
            UsbReadOutcome::Retry(rusb::Error::Overflow)
        );
    }
    assert_eq!(
        classify_usb_read(Err(rusb::Error::Pipe), &mut errors),
        UsbReadOutcome::Stop(rusb::Error::Pipe)
    );
    let mut fresh = 0_u32;
    assert_eq!(
        classify_usb_read(Err(rusb::Error::NoDevice), &mut fresh),
        UsbReadOutcome::Stop(rusb::Error::NoDevice)
    );
}
