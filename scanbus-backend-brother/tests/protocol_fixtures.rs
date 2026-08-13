//! The skey parsers against payloads they did not produce.
//!
//! The unit tests in `src/skey/` build their own inputs, which proves internal
//! consistency and nothing else. These read bytes recovered from `brscan-skey` itself —
//! see `tests/fixtures/README.md` for where each one came from, and for what still needs
//! a packet capture that no test here can stand in for.

use std::net::Ipv4Addr;
use std::time::Duration;

use scanbus_backend_brother::skey::event::{Event, Frame, HEADER_LEN};
use scanbus_backend_brother::skey::register::{Function, Registration, UserName};

const KEYPRESS_HEADER: &[u8] = include_bytes!("fixtures/keypress-image.header");
const KEYPRESS_PAYLOAD: &[u8] = include_bytes!("fixtures/keypress-image.payload");
const REGISTRATION_VALUE: &str = include_str!("fixtures/registration-image.value");

/// Brother's own sample key press, parsed to the button it names.
///
/// The header is reframed rather than used verbatim: the compiled-in one declares 0x0074
/// bytes over a 137-byte payload, because the `b[0] == 0x02` branch of `check_udp_data`
/// returns before the length is ever checked. That inconsistency is itself worth pinning
/// down — if a future `brscan-skey` ships a coherent blob, this assertion is where we
/// find out.
#[test]
fn the_vendors_compiled_in_sample_key_press_parses() {
    assert_eq!(KEYPRESS_HEADER, &[0x02, 0x00, 0x74, 0x30]);
    let declared = usize::from(KEYPRESS_HEADER[1]) << 8 | usize::from(KEYPRESS_HEADER[2]);
    assert_ne!(
        declared,
        KEYPRESS_PAYLOAD.len(),
        "the compiled-in header is inconsistent on purpose; if that changed, so did the \
         framing this crate implements"
    );

    let payload = std::str::from_utf8(KEYPRESS_PAYLOAD).unwrap();
    let datagram = Frame {
        id: 0x01,
        code: 0x01,
        payload,
    }
    .to_datagram();
    assert_eq!(datagram.len(), HEADER_LEN + KEYPRESS_PAYLOAD.len());

    let Event::KeyPress(press) = Event::parse(&datagram).unwrap() else {
        panic!("the sample is a key press");
    };
    assert_eq!(press.function, Function::Image);
    assert_eq!(press.function.button_index(), 1);
    assert_eq!(press.user, "idevd101");
    assert_eq!(press.client, Ipv4Addr::new(10, 136, 41, 234));
    assert_eq!(press.host.as_deref(), Some("10.136.150.6:54925"));
    assert_eq!(press.appnum_agrees(), Some(true));
}

/// The unused fields in that payload — `P1`…`P4`, `REGID`, `SEQ`, and the stray empty
/// field before `CLIENT` — must not turn a good datagram into a parse error.
#[test]
fn fields_the_parser_has_no_use_for_are_ignored_rather_than_fatal() {
    let payload = std::str::from_utf8(KEYPRESS_PAYLOAD).unwrap();
    assert!(
        payload.contains(";;CLIENT="),
        "the stray empty field is the point"
    );
    assert!(payload.contains("P4=0;"));
    assert!(payload.contains("REGID=756;"));

    let datagram = Frame {
        id: 0x01,
        code: 0x01,
        payload,
    }
    .to_datagram();
    assert!(matches!(Event::parse(&datagram), Ok(Event::KeyPress(_))));
}

/// A registration built by this crate is byte-identical to the vendor's format string
/// filled in with the same arguments.
#[test]
fn the_registration_string_matches_the_vendor_format() {
    let registration = Registration::new(
        UserName::new("jean").unwrap(),
        Ipv4Addr::new(192, 168, 1, 20),
        Function::Image,
    );
    assert_eq!(registration.value().unwrap(), REGISTRATION_VALUE);

    // …and reading it back gives the registration it was built from.
    let parsed = Registration::parse(REGISTRATION_VALUE).unwrap();
    assert_eq!(parsed, registration);
    assert_eq!(parsed.duration, Duration::from_secs(360));
}

/// Every truncation of the fixture datagram, since this is the shape of the bytes that
/// will actually arrive on a socket bound to a well-known port.
#[test]
fn no_prefix_of_the_fixture_datagram_parses_or_panics() {
    let payload = std::str::from_utf8(KEYPRESS_PAYLOAD).unwrap();
    let datagram = Frame {
        id: 0x01,
        code: 0x01,
        payload,
    }
    .to_datagram();

    for len in 0..datagram.len() {
        assert!(
            Event::parse(&datagram[..len]).is_err(),
            "a {len}-byte prefix parsed as a whole datagram"
        );
    }
    assert!(Event::parse(&datagram).is_ok());
}
