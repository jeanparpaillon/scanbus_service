//! The UDP/54925 datagram: what the device sends when a panel entry is chosen.
//!
//! # Framing
//!
//! Four binary bytes, then an ASCII payload. From `udp_sent` at `0x40884f`, which is the
//! vendor's own writer for this socket:
//!
//! ```text
//! byte 0   id
//! byte 1   length >> 8
//! byte 2   length            length == strlen(payload)
//! byte 3   code
//! byte 4…  payload           NUL-terminated in memory; the NUL is not sent
//! ```
//!
//! `check_udp_data` at `0x408134` validates `(b[1] << 8) | b[2] == strlen(b + 4)` and then
//! dispatches on `(b[0], b[3])`. A **panel key press is `id = 0x01, code = 0x01`**; every
//! other pair the daemon recognises is an internal command it sends to *itself* over the
//! loopback, because the CLI (`brscan-skey -t`, `-a`, `-l`, `--refresh`) talks to the
//! running daemon through this same socket. Those are enumerated in [`InternalCommand`]
//! rather than ignored: scanbus binds the port, so it will receive them if a user runs
//! the vendor CLI, and "a datagram I understand and decline to act on" is a better answer
//! than a parse error.
//!
//! # The payload
//!
//! The same `KEY=VALUE;` grammar as the registration string. `decode_key_data` at
//! `0x40bad1` reads `USER=`, `TYPE=` (must be `BR`), `HOST=`, `CLIENT=` and `FUNC=`;
//! `check_udp_data` separately reads `IP=`, `NODENAME=` and `DEVICE=` for the device's
//! name. The sample datagram Brother compiled into `.data` at `0x61b480` shows the fuller
//! set the firmware sends:
//!
//! ```text
//! TYPE=BR;BUTTON=SCAN;USER="idevd101";FUNC=IMAGE;HOST=10.136.150.6:54925;APPNUM=1;
//! P1=0;P2=0;P3=0;P4=0;REGID=756;SEQ=1;;CLIENT=10.136.41.234
//! ```
//!
//! Field *order* is therefore not something to depend on — the vendor does not — so
//! [`KeyPress::parse`] is order-independent and ignores fields it has no use for
//! (`P1`…`P4`, `REGID`, `SEQ`, and the stray empty field `;;` in the sample above).
//!
//! # No reply
//!
//! The device is not waiting for an acknowledgement on this socket. After
//! `check_udp_data`, `udp_agent` (`0x407b15`) calls only `set_sync_event`,
//! `get_last_recv_ip_address`, `sprintf` and `strcpy`; there is no path from a received
//! datagram to `udp_sent`. The only datagram the daemon writes is the `Refresh Device
//! List` command it posts to itself when the receive loop times out.

use std::fmt;
use std::net::Ipv4Addr;

use super::fields::field;
use super::register::Function;

/// The four binary bytes in front of every payload.
pub const HEADER_LEN: usize = 4;

/// `id` and `code` of a panel key press.
const KEY_PRESS_ID: u8 = 0x01;
const KEY_PRESS_CODE: u8 = 0x01;

/// `id` shared by every command the vendor CLI posts to its own daemon.
const INTERNAL_ID: u8 = 0x80;

/// Why a datagram is not an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventError {
    /// Fewer bytes than the header needs.
    TooShort(usize),
    /// The declared payload length is not the payload's length.
    LengthMismatch { declared: usize, actual: usize },
    /// A NUL inside the payload, which would make `strlen` and the length field disagree
    /// on the sender's side too.
    EmbeddedNul,
    /// The payload is not text.
    NotUtf8,
    /// An `(id, code)` pair no version of `check_udp_data` recognises.
    UnknownFrame { id: u8, code: u8 },
    /// A key press with a field missing.
    MissingField(&'static str),
    /// A key press with a field that is present and wrong.
    BadField { field: &'static str, value: String },
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort(len) => write!(
                f,
                "datagram is {len} bytes; the skey header alone is {HEADER_LEN}"
            ),
            Self::LengthMismatch { declared, actual } => write!(
                f,
                "datagram header declares a {declared}-byte payload, {actual} bytes follow"
            ),
            Self::EmbeddedNul => f.write_str("skey payload contains a NUL"),
            Self::NotUtf8 => f.write_str("skey payload is not UTF-8"),
            Self::UnknownFrame { id, code } => {
                write!(f, "unknown skey frame: id {id:#04x}, code {code:#04x}")
            }
            Self::MissingField(name) => write!(f, "key press datagram has no {name}= field"),
            Self::BadField { field, value } => {
                write!(f, "key press field {field}={value:?} is not valid")
            }
        }
    }
}

impl std::error::Error for EventError {}

/// A datagram, after framing and before interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub id: u8,
    pub code: u8,
    pub payload: &'a str,
}

impl<'a> Frame<'a> {
    /// Split the header off a datagram and check it against the payload.
    ///
    /// A single trailing NUL is tolerated. `udp_sent` sends `4 + strlen` and stops short
    /// of the terminator, but the firmware is a different implementation of the same
    /// frame, and a NUL-terminated variant would otherwise be rejected as a length
    /// mismatch — a failure whose message would send someone looking in the wrong place.
    /// Anything else after the declared payload is a real disagreement about framing.
    pub fn parse(datagram: &'a [u8]) -> Result<Self, EventError> {
        if datagram.len() < HEADER_LEN {
            return Err(EventError::TooShort(datagram.len()));
        }
        let declared = usize::from(datagram[1]) << 8 | usize::from(datagram[2]);
        let body = &datagram[HEADER_LEN..];

        let payload = match body.len().checked_sub(declared) {
            Some(0) => body,
            Some(1) if body[declared] == 0 => &body[..declared],
            _ => {
                return Err(EventError::LengthMismatch {
                    declared,
                    actual: body.len(),
                });
            }
        };
        if payload.contains(&0) {
            return Err(EventError::EmbeddedNul);
        }

        Ok(Self {
            id: datagram[0],
            code: datagram[3],
            payload: std::str::from_utf8(payload).map_err(|_| EventError::NotUtf8)?,
        })
    }

    /// Build a frame, for tests and for the loopback commands scanbus may one day answer.
    pub fn to_datagram(&self) -> Vec<u8> {
        let payload = self.payload.as_bytes();
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(self.id);
        out.push((payload.len() >> 8) as u8);
        out.push(payload.len() as u8);
        out.push(self.code);
        out.extend_from_slice(payload);
        out
    }
}

/// A command the vendor CLI sends to its own daemon over this socket.
///
/// scanbus acts on none of them; the variant exists so that receiving one is a recognised
/// no-op rather than a parse failure logged as a malformed device datagram.
///
/// The `$1`/`$2`/`$3Debug` frames are deliberately absent: `check_udp_data` recognises
/// them by their payload with no constraint on `code` at all, so there is no `(id, code)`
/// that identifies one. They fall through to [`EventError::UnknownFrame`], which is the
/// right answer for a debugging aid aimed at another daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalCommand {
    AddDevice,
    DeleteDevice,
    Terminate,
    ListDevices,
    Refresh,
}

impl InternalCommand {
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0x80 => Some(Self::AddDevice),
            0x81 => Some(Self::DeleteDevice),
            0x82 => Some(Self::Terminate),
            0x83 => Some(Self::ListDevices),
            0x84 => Some(Self::Refresh),
            _ => None,
        }
    }
}

/// A panel key press, as much of it as is useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPress {
    /// `USER=` — the destination name this host registered under. The vendor compares it
    /// to the login user and drops the datagram on a mismatch; here it is carried so the
    /// caller can decide, because scanbus registers a sanitised name that need not be the
    /// login name.
    pub user: String,
    /// `FUNC=` — which panel entry was chosen.
    pub function: Function,
    /// `CLIENT=` — the device's own address, and the only field that says *which* scanner
    /// this is. It is what the singleton listener demultiplexes on.
    pub client: Ipv4Addr,
    /// `HOST=` — the address the device believes it is talking to. Worth keeping: a
    /// mismatch with any of our addresses is exactly the multi-homed/VPN failure the
    /// design warns about, and it is diagnosable only from here.
    pub host: Option<String>,
    /// `APPNUM=` when present. Redundant with `function`, and a disagreement between the
    /// two is worth noticing rather than smoothing over.
    pub appnum: Option<u8>,
    /// `DEVICE=`, else `NODENAME=`, else `IP=` — the vendor's own precedence, from the
    /// order of the three `strstr` calls in `check_udp_data`, where the last match wins.
    pub device: Option<String>,
}

impl KeyPress {
    fn parse(payload: &str) -> Result<Self, EventError> {
        let expect =
            |name: &'static str| field(payload, name).ok_or(EventError::MissingField(name));
        let bad = |name: &'static str, found: &str| EventError::BadField {
            field: name,
            value: found.to_owned(),
        };

        let type_ = expect("TYPE")?;
        if type_ != "BR" {
            return Err(bad("TYPE", type_));
        }

        let func = expect("FUNC")?;
        let function = Function::from_token(func).ok_or_else(|| bad("FUNC", func))?;

        let client_field = expect("CLIENT")?;
        let client = client_field
            .parse::<Ipv4Addr>()
            .map_err(|_| bad("CLIENT", client_field))?;

        let appnum = match field(payload, "APPNUM") {
            Some(value) => Some(value.parse::<u8>().map_err(|_| bad("APPNUM", value))?),
            None => None,
        };

        Ok(Self {
            user: expect("USER")?.to_owned(),
            function,
            client,
            host: field(payload, "HOST").map(str::to_owned),
            appnum,
            device: field(payload, "DEVICE")
                .or_else(|| field(payload, "NODENAME"))
                .or_else(|| field(payload, "IP"))
                .map(str::to_owned),
        })
    }

    /// Whether `APPNUM=` agrees with `FUNC=`. `None` when the datagram carried no APPNUM.
    pub fn appnum_agrees(&self) -> Option<bool> {
        self.appnum.map(|appnum| appnum == self.function.appnum())
    }
}

/// Everything a datagram on this socket can turn out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    KeyPress(KeyPress),
    Internal(InternalCommand),
}

impl Event {
    pub fn parse(datagram: &[u8]) -> Result<Self, EventError> {
        let frame = Frame::parse(datagram)?;
        match (frame.id, frame.code) {
            (KEY_PRESS_ID, KEY_PRESS_CODE) => Ok(Self::KeyPress(KeyPress::parse(frame.payload)?)),
            (INTERNAL_ID, code) => InternalCommand::from_code(code).map(Self::Internal).ok_or(
                EventError::UnknownFrame {
                    id: frame.id,
                    code: frame.code,
                },
            ),
            // `$1/$2/$3Debug` frames carry an id of 0x80 with an unconstrained code, and
            // the `0x02`/`0x30` pair is the canned debug-client datagram. Neither reaches
            // us from a device.
            (id, code) => Err(EventError::UnknownFrame { id, code }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload Brother compiled into `.data` at `0x61b480`, re-framed with a header
    /// that is internally consistent — the canned one is not, because the branch it was
    /// written for returns before `check_udp_data` checks the length.
    const SAMPLE_PAYLOAD: &str = "TYPE=BR;BUTTON=SCAN;USER=\"idevd101\";FUNC=IMAGE;\
                                  HOST=10.136.150.6:54925;APPNUM=1;P1=0;P2=0;P3=0;P4=0;\
                                  REGID=756;SEQ=1;;CLIENT=10.136.41.234";

    fn key_press_datagram(payload: &str) -> Vec<u8> {
        Frame {
            id: KEY_PRESS_ID,
            code: KEY_PRESS_CODE,
            payload,
        }
        .to_datagram()
    }

    #[test]
    fn the_vendors_own_sample_payload_parses_to_the_right_function() {
        let Event::KeyPress(press) = Event::parse(&key_press_datagram(SAMPLE_PAYLOAD)).unwrap()
        else {
            panic!("the sample is a key press");
        };
        assert_eq!(press.function, Function::Image);
        assert_eq!(press.user, "idevd101");
        assert_eq!(press.client, Ipv4Addr::new(10, 136, 41, 234));
        assert_eq!(press.host.as_deref(), Some("10.136.150.6:54925"));
        assert_eq!(press.appnum, Some(1));
        assert_eq!(press.appnum_agrees(), Some(true));
    }

    /// Each of the four panel entries, and the button index the API will publish for it.
    #[test]
    fn each_function_press_parses_to_its_own_button() {
        for function in Function::ALL {
            let payload = format!(
                "IP=192.168.1.3:54925;NODENAME=\"BRW001122334455\";TYPE=BR;BUTTON=SCAN;\
                 USER=\"desktop\";FUNC={};HOST=192.168.1.20:54925;APPNUM={};P1=0;P2=0;\
                 P3=0;P4=0;REGID=1;SEQ=1;;CLIENT=192.168.1.3",
                function.as_str(),
                function.appnum()
            );
            let Event::KeyPress(press) = Event::parse(&key_press_datagram(&payload)).unwrap()
            else {
                panic!("expected a key press for {function}");
            };
            assert_eq!(press.function, function);
            assert_eq!(press.function.button_index(), function.button_index());
            assert_eq!(press.appnum_agrees(), Some(true));
            assert_eq!(press.client, Ipv4Addr::new(192, 168, 1, 3));
            // NODENAME wins over IP, as it does in `check_udp_data`.
            assert_eq!(press.device.as_deref(), Some("BRW001122334455"));
        }
    }

    #[test]
    fn device_is_taken_in_the_vendors_precedence() {
        let with_device = "DEVICE=MFC;NODENAME=BRW;IP=1.2.3.4;TYPE=BR;USER=pc;FUNC=FILE;\
                           CLIENT=1.2.3.4";
        let Event::KeyPress(press) = Event::parse(&key_press_datagram(with_device)).unwrap() else {
            panic!("key press");
        };
        assert_eq!(press.device.as_deref(), Some("MFC"));

        let ip_only = "IP=1.2.3.4:54925;TYPE=BR;USER=pc;FUNC=FILE;CLIENT=1.2.3.4";
        let Event::KeyPress(press) = Event::parse(&key_press_datagram(ip_only)).unwrap() else {
            panic!("key press");
        };
        assert_eq!(press.device.as_deref(), Some("1.2.3.4:54925"));
    }

    /// A disagreement between the two ways the datagram names the function is reported,
    /// not silently resolved in favour of one of them.
    #[test]
    fn an_appnum_that_contradicts_func_is_visible() {
        let payload = "TYPE=BR;USER=pc;FUNC=FILE;APPNUM=1;CLIENT=1.2.3.4";
        let Event::KeyPress(press) = Event::parse(&key_press_datagram(payload)).unwrap() else {
            panic!("key press");
        };
        assert_eq!(press.function, Function::File);
        assert_eq!(press.appnum, Some(1));
        assert_eq!(press.appnum_agrees(), Some(false));
    }

    /// The property that matters for a socket bound to a well-known port: nothing panics,
    /// and nothing short of a whole datagram is accepted as one.
    #[test]
    fn every_prefix_of_a_key_press_is_an_error_not_a_panic() {
        let datagram = key_press_datagram(SAMPLE_PAYLOAD);
        for len in 0..datagram.len() {
            assert!(
                Event::parse(&datagram[..len]).is_err(),
                "a {len}-byte prefix parsed as a whole datagram"
            );
        }
        assert!(Event::parse(&datagram).is_ok());
    }

    #[test]
    fn every_truncation_and_extension_of_the_declared_length_is_an_error() {
        let datagram = key_press_datagram(SAMPLE_PAYLOAD);

        let mut long = datagram.clone();
        long.push(b'x');
        assert!(matches!(
            Event::parse(&long),
            Err(EventError::LengthMismatch { .. })
        ));

        // …except exactly one NUL, which is the terminator a different implementation of
        // the same frame would plausibly include.
        let mut terminated = datagram.clone();
        terminated.push(0);
        assert!(matches!(Event::parse(&terminated), Ok(Event::KeyPress(_))));

        let mut two_nuls = terminated.clone();
        two_nuls.push(0);
        assert!(matches!(
            Event::parse(&two_nuls),
            Err(EventError::LengthMismatch { .. })
        ));

        // A length field that lies in the other direction.
        let mut overlong = datagram.clone();
        overlong[1] = 0xff;
        assert!(matches!(
            Event::parse(&overlong),
            Err(EventError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn garbage_is_an_error_rather_than_a_panic() {
        assert_eq!(Event::parse(&[]), Err(EventError::TooShort(0)));
        assert_eq!(Event::parse(&[1, 2, 3]), Err(EventError::TooShort(3)));

        // Right framing, unknown frame type.
        assert_eq!(
            Event::parse(&[0x7f, 0x00, 0x01, 0x7f, b'x']),
            Err(EventError::UnknownFrame {
                id: 0x7f,
                code: 0x7f
            })
        );
        // The canned debug-client frame from `.data` is not something a device sends.
        assert!(matches!(
            Event::parse(&[0x02, 0x00, 0x01, 0x30, b'0']),
            Err(EventError::UnknownFrame { .. })
        ));
        // Non-text payload.
        assert_eq!(
            Event::parse(&[KEY_PRESS_ID, 0x00, 0x02, KEY_PRESS_CODE, 0xff, 0xfe]),
            Err(EventError::NotUtf8)
        );
        // A NUL inside the payload: `strlen` on the sender's side would have disagreed
        // with the length it wrote.
        assert_eq!(
            Event::parse(&[KEY_PRESS_ID, 0x00, 0x03, KEY_PRESS_CODE, b'a', 0x00, b'b']),
            Err(EventError::EmbeddedNul)
        );
    }

    #[test]
    fn a_key_press_missing_or_contradicting_a_required_field_is_named() {
        assert_eq!(
            Event::parse(&key_press_datagram("USER=pc;FUNC=FILE;CLIENT=1.2.3.4")),
            Err(EventError::MissingField("TYPE"))
        );
        assert_eq!(
            Event::parse(&key_press_datagram("TYPE=BR;USER=pc;FUNC=FILE")),
            Err(EventError::MissingField("CLIENT"))
        );
        assert_eq!(
            Event::parse(&key_press_datagram(
                "TYPE=XX;USER=pc;FUNC=FILE;CLIENT=1.2.3.4"
            )),
            Err(EventError::BadField {
                field: "TYPE",
                value: "XX".to_owned()
            })
        );
        assert_eq!(
            Event::parse(&key_press_datagram(
                "TYPE=BR;USER=pc;FUNC=FAX;CLIENT=1.2.3.4"
            )),
            Err(EventError::BadField {
                field: "FUNC",
                value: "FAX".to_owned()
            })
        );
        assert_eq!(
            Event::parse(&key_press_datagram("TYPE=BR;USER=pc;FUNC=FILE;CLIENT=nope")),
            Err(EventError::BadField {
                field: "CLIENT",
                value: "nope".to_owned()
            })
        );
    }

    /// Somebody runs `brscan-skey --refresh` while scanbus holds the port. That is a
    /// datagram we understand and decline, not a malformed device message.
    #[test]
    fn the_vendor_clis_own_commands_are_recognised_and_declined() {
        let cases = [
            (
                0x80u8,
                "Add device:Internal command",
                InternalCommand::AddDevice,
            ),
            (
                0x81,
                "Del device:Internal command",
                InternalCommand::DeleteDevice,
            ),
            (
                0x82,
                "Terminate:Internal command",
                InternalCommand::Terminate,
            ),
            (
                0x83,
                "List Device:Internal command",
                InternalCommand::ListDevices,
            ),
            (0x84, "Refresh Device List", InternalCommand::Refresh),
        ];
        for (code, payload, expected) in cases {
            let datagram = Frame {
                id: INTERNAL_ID,
                code,
                payload,
            }
            .to_datagram();
            assert_eq!(
                Event::parse(&datagram).unwrap(),
                Event::Internal(expected),
                "code {code:#04x}"
            );
        }
        // An id of 0x80 with a code nothing claims.
        assert!(matches!(
            Event::parse(
                &Frame {
                    id: INTERNAL_ID,
                    code: 0x8f,
                    payload: "x"
                }
                .to_datagram()
            ),
            Err(EventError::UnknownFrame { .. })
        ));
    }

    #[test]
    fn framing_round_trips_including_a_payload_past_the_one_byte_length() {
        for len in [0usize, 1, 255, 256, 1000] {
            let payload = "y".repeat(len);
            let frame = Frame {
                id: KEY_PRESS_ID,
                code: KEY_PRESS_CODE,
                payload: &payload,
            };
            let datagram = frame.to_datagram();
            assert_eq!(datagram.len(), HEADER_LEN + len);
            assert_eq!(Frame::parse(&datagram).unwrap(), frame, "length {len}");
        }
    }

    #[test]
    fn every_single_byte_corruption_of_a_key_press_is_handled() {
        let datagram = key_press_datagram(SAMPLE_PAYLOAD);
        for index in 0..datagram.len() {
            for replacement in [0x00u8, 0x01, 0x3b, 0x80, 0xff] {
                let mut corrupt = datagram.clone();
                corrupt[index] = replacement;
                let _ = Event::parse(&corrupt);
            }
        }
    }
}
