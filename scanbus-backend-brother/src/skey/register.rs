//! The registration string, and the `SetRequest` that carries it.
//!
//! Registering is how this host appears under *Scan to PC* on the panel. One registration
//! per function, each an SNMP `SetRequest` writing a `;`-delimited string to
//! [`REGISTRATION_OID`]:
//!
//! ```text
//! TYPE=BR;BUTTON=SCAN;USER="jean";FUNC=IMAGE;HOST=192.168.1.20:54925;APPNUM=1;DURATION=360;BRID=;
//! ```
//!
//! That is `brscan-skey`'s own format string, at `0x414a78` in the shipped binary, filled
//! in by the `sprintf` at `0x406efe` from — in argument order — the login user, the
//! function name, `ip:54925`, the APPNUM, the `duration` global, and BRID.
//!
//! # `DURATION` is a lease, so registering is a task and not a call
//!
//! The device drops the entry when the lease runs out. That is the behaviour we want
//! ([`brother-skeyless-backend.md`] §2.2): a daemon that dies stops appearing on the
//! panel by itself, and unmapping a button is implemented by *not refreshing* it rather
//! than by sending anything.
//!
//! # What we do not reproduce
//!
//! `BRID` is the four-digit panel password, run through Brother's own scrambler
//! (`scramble.c`; `enc_main` at `0x406aaa`, tables `shuffletbl`/`hextbl` and the key blob
//! at `0x414a70`). scanbus does not implement that obfuscation and does not offer the
//! feature: [`Registration::new`] always writes `BRID=`, which is what the vendor also
//! writes when no password is configured — and `password=` empty is how `brscan-skey.config`
//! ships. Anyone who has set a panel password on the device has a fourth field we cannot
//! fill, and registration will simply not take; that belongs in the degraded-path list,
//! not in a reimplementation of an obfuscation.
//!
//! [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

use super::fields::field;
use super::snmp::{Message, Oid};

/// Brother's private-enterprise OID for scan-key registration.
///
/// `1.3.6.1.4.1.2435` is Brother Industries; the rest is the scan-key subtree. Confirmed
/// present on the development machine's MFC-J5335DW, where a read returns `TRUE` rather
/// than `noSuchName`, and the same constant the vendor writes to on both its transports —
/// it is spelled out in the Phoenix JSON template at `0x415a90` as well.
pub const REGISTRATION_OID: &str = "1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0";

/// The port the device sends a panel event to. Fixed, well-known, one per host.
pub const LISTENER_PORT: u16 = 54925;

/// The lease the vendor asks for, from the `duration` global at `0x61b320`.
pub const DEFAULT_DURATION: Duration = Duration::from_secs(360);

/// Brother documents a scan-destination name of "up to 15 alphanumeric characters".
pub const MAX_USER_LEN: usize = 15;

/// What a panel entry does when it is chosen.
///
/// The three numbers attached to each are all different and all load-bearing, which is
/// why they are methods here rather than something a call site works out:
/// [`Function::appnum`] goes on the wire, [`Function::button_index`] is the scanbus API's
/// `Button1` index (§5), and the vendor's own `decode_key_data` uses a *third* order
/// (IMAGE 0, OCR 1, EMAIL 2, FILE 3) internally, which this crate does not adopt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Function {
    File,
    Image,
    Ocr,
    Email,
}

impl Function {
    /// Every function, in `button_index` order.
    pub const ALL: [Self; 4] = [Self::File, Self::Image, Self::Ocr, Self::Email];

    /// The `FUNC=` token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "FILE",
            Self::Image => "IMAGE",
            Self::Ocr => "OCR",
            Self::Email => "EMAIL",
        }
    }

    /// The `APPNUM=` token. Not an index — the values are 1, 3, 2, 5 and skip 4.
    pub const fn appnum(self) -> u8 {
        match self {
            Self::Image => 1,
            Self::Email => 2,
            Self::Ocr => 3,
            Self::File => 5,
        }
    }

    /// The scanbus `Button1` index, per the table in `brother-skeyless-backend.md` §3.
    pub const fn button_index(self) -> u32 {
        match self {
            Self::File => 0,
            Self::Image => 1,
            Self::Ocr => 2,
            Self::Email => 3,
        }
    }

    /// What the panel calls this entry. The firmware's wording, from the strings the
    /// daemon prints for the same four codes; `LabelConfigurable` stays `false` because
    /// nothing we send changes it.
    pub const fn device_label(self) -> &'static str {
        match self {
            Self::File => "Scan to File",
            Self::Image => "Scan to Image",
            Self::Ocr => "Scan to OCR",
            Self::Email => "Scan to E-Mail",
        }
    }

    pub fn from_button_index(index: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.button_index() == index)
    }

    pub fn from_appnum(appnum: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.appnum() == appnum)
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == token)
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything that can be wrong about a registration, before it reaches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The `USER=` value would not survive the device's own limits.
    UserName { name: String, reason: &'static str },
    /// A `DURATION=` the device's field cannot express.
    Duration(u64),
    /// A field a registration string cannot do without.
    MissingField(&'static str),
    /// A field that is present and not what it has to be.
    BadField { field: &'static str, value: String },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserName { name, reason } => write!(
                f,
                "{name:?} cannot be used as a Brother scan destination name: {reason}"
            ),
            Self::Duration(secs) => write!(
                f,
                "a lease of {secs} s does not fit Brother's DURATION field (1..={})",
                u32::MAX
            ),
            Self::MissingField(field) => {
                write!(f, "registration string has no {field}= field")
            }
            Self::BadField { field, value } => {
                write!(f, "registration field {field}={value:?} is not valid")
            }
        }
    }
}

impl std::error::Error for RegisterError {}

/// A destination name the device will accept.
///
/// Checked rather than assumed. Brother's own documentation of the Scan Key Tool gives
/// the rule — at most 15 alphanumeric characters — and a name that breaks it does not
/// fail loudly: the entry either does not appear on the panel or appears mangled, hours
/// after the registration was accepted. A login name like `jean.parpaillon` or a hostname
/// with a `-` in it trips this on a perfectly ordinary machine, so the error says which
/// rule was broken and the caller is expected to sanitise, not to guess.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserName(String);

impl UserName {
    pub fn new(name: impl Into<String>) -> Result<Self, RegisterError> {
        let name = name.into();
        let reason = if name.is_empty() {
            Some("it is empty")
        } else if name.chars().count() > MAX_USER_LEN {
            Some("it is longer than 15 characters")
        } else if !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            Some("it contains something other than ASCII letters and digits")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(RegisterError::UserName { name, reason }),
            None => Ok(Self(name)),
        }
    }

    /// The longest prefix of `candidate` that is a valid name, or `None` if nothing is.
    ///
    /// For turning a login name or hostname into something registrable without asking the
    /// user to invent one. `jean.parpaillon` becomes `jeanparpaillon`; a name of nothing
    /// but punctuation has no answer and gets `None` rather than an empty registration.
    pub fn sanitised(candidate: &str) -> Option<Self> {
        let kept: String = candidate
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(MAX_USER_LEN)
            .collect();
        Self::new(kept).ok()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One registration: this host, under one function, for one lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub user: UserName,
    /// The address the device will send the event to.
    ///
    /// Must be the address of the interface that routes to *that device*, not the first
    /// non-loopback one — a machine on a VPN otherwise registers an address the printer
    /// cannot reach, and the panel entry appears and then does nothing.
    pub host: Ipv4Addr,
    pub port: u16,
    pub function: Function,
    pub duration: Duration,
}

impl Registration {
    pub fn new(user: UserName, host: Ipv4Addr, function: Function) -> Self {
        Self {
            user,
            host,
            port: LISTENER_PORT,
            function,
            duration: DEFAULT_DURATION,
        }
    }

    /// The string that goes in the varbind, byte for byte as the vendor writes it.
    ///
    /// Field order is not cosmetic: it is the order of `0x414a78`, and there is no
    /// evidence the firmware parses order-independently the way our own reader does.
    pub fn value(&self) -> Result<String, RegisterError> {
        let seconds = self.duration.as_secs();
        if seconds == 0 || seconds > u64::from(u32::MAX) {
            return Err(RegisterError::Duration(seconds));
        }
        Ok(format!(
            "TYPE=BR;BUTTON=SCAN;USER=\"{user}\";FUNC={func};HOST={host}:{port};\
             APPNUM={appnum};DURATION={seconds};BRID=;",
            user = self.user,
            func = self.function.as_str(),
            host = self.host,
            port = self.port,
            appnum = self.function.appnum(),
        ))
    }

    /// Read a registration back — for tests, and for reading what a device says it holds.
    ///
    /// Accepts both spellings of `USER`: quoted, as the SNMP path writes it, and bare, as
    /// the Phoenix JSON path at `0x415a90` does.
    pub fn parse(value: &str) -> Result<Self, RegisterError> {
        let expect =
            |name: &'static str| field(value, name).ok_or(RegisterError::MissingField(name));
        let bad = |name: &'static str, found: &str| RegisterError::BadField {
            field: name,
            value: found.to_owned(),
        };

        let type_ = expect("TYPE")?;
        if type_ != "BR" {
            return Err(bad("TYPE", type_));
        }
        let user = UserName::new(expect("USER")?)?;
        let func = expect("FUNC")?;
        let function = Function::from_token(func).ok_or_else(|| bad("FUNC", func))?;

        let host_field = expect("HOST")?;
        let (host, port) = match host_field.split_once(':') {
            Some((host, port)) => (
                host,
                port.parse::<u16>().map_err(|_| bad("HOST", host_field))?,
            ),
            // `InitOID_MessPtr` appends `:54925` when the configured host has no port, so
            // a bare address means the default rather than an error.
            None => (host_field, LISTENER_PORT),
        };
        let host = host
            .parse::<Ipv4Addr>()
            .map_err(|_| bad("HOST", host_field))?;

        let duration_field = expect("DURATION")?;
        let duration = duration_field
            .parse::<u32>()
            .map_err(|_| bad("DURATION", duration_field))?;

        Ok(Self {
            user,
            host,
            port,
            function,
            duration: Duration::from_secs(u64::from(duration)),
        })
    }

    /// The `SetRequest` that installs this registration.
    pub fn set_request(&self, community: &str, request_id: i32) -> Result<Message, RegisterError> {
        let oid: Oid = REGISTRATION_OID
            .parse()
            .expect("the registration OID is a compile-time constant and parses");
        Ok(Message::set(community, request_id, oid, self.value()?))
    }
}

/// The read that tells a device apart from one that has never heard of the OID.
///
/// A `Response` with `error-status` `noSuchName` is the "older or newer generation"
/// degraded path: the scanner stays usable as a pull scanner and reports zero buttons.
pub fn probe_request(community: &str, request_id: i32) -> Message {
    let oid: Oid = REGISTRATION_OID
        .parse()
        .expect("the registration OID is a compile-time constant and parses");
    Message::get(community, request_id, oid)
}

#[cfg(test)]
mod tests {
    use super::super::snmp;
    use super::*;

    fn user(name: &str) -> UserName {
        UserName::new(name).unwrap()
    }

    /// The exact string, against the vendor's format string rather than against us.
    #[test]
    fn the_registration_string_is_the_one_brscan_skey_writes() {
        let registration = Registration::new(
            user("jean"),
            Ipv4Addr::new(192, 168, 1, 20),
            Function::Image,
        );
        assert_eq!(
            registration.value().unwrap(),
            "TYPE=BR;BUTTON=SCAN;USER=\"jean\";FUNC=IMAGE;HOST=192.168.1.20:54925;\
             APPNUM=1;DURATION=360;BRID=;"
        );
    }

    /// The four panel entries, with the three different numberings kept apart.
    #[test]
    fn each_function_carries_its_own_appnum_and_button_index() {
        let expected = [
            (Function::File, "FILE", 5, 0, "Scan to File"),
            (Function::Image, "IMAGE", 1, 1, "Scan to Image"),
            (Function::Ocr, "OCR", 3, 2, "Scan to OCR"),
            (Function::Email, "EMAIL", 2, 3, "Scan to E-Mail"),
        ];
        for (function, token, appnum, index, label) in expected {
            assert_eq!(function.as_str(), token);
            assert_eq!(function.appnum(), appnum);
            assert_eq!(function.button_index(), index);
            assert_eq!(function.device_label(), label);
            assert_eq!(Function::from_token(token), Some(function));
            assert_eq!(Function::from_appnum(appnum), Some(function));
            assert_eq!(Function::from_button_index(index), Some(function));

            let value = Registration::new(user("host"), Ipv4Addr::LOCALHOST, function)
                .value()
                .unwrap();
            assert!(value.contains(&format!("FUNC={token};")), "{value}");
            assert!(value.contains(&format!("APPNUM={appnum};")), "{value}");
        }
        // APPNUM 4 is not a function: the panel entries are 1, 2, 3 and 5.
        assert_eq!(Function::from_appnum(4), None);
        assert_eq!(Function::from_button_index(4), None);
        assert_eq!(Function::from_token("SCAN"), None);
    }

    #[test]
    fn every_registration_round_trips_through_its_own_string() {
        for function in Function::ALL {
            let registration = Registration {
                user: user("desktop01"),
                host: Ipv4Addr::new(10, 0, 0, 7),
                port: LISTENER_PORT,
                function,
                duration: Duration::from_secs(120),
            };
            let parsed = Registration::parse(&registration.value().unwrap()).unwrap();
            assert_eq!(parsed, registration);
        }
    }

    /// The Phoenix template writes `USER=%s` bare. Same field, same meaning.
    #[test]
    fn an_unquoted_user_field_parses_the_same_as_a_quoted_one() {
        let quoted = "TYPE=BR;BUTTON=SCAN;USER=\"jean\";FUNC=FILE;HOST=192.168.1.20:54925;\
                      APPNUM=5;DURATION=360;BRID=;";
        let bare = "TYPE=BR;BUTTON=SCAN;USER=jean;FUNC=FILE;HOST=192.168.1.20:54925;\
                    APPNUM=5;DURATION=360;BRID=;";
        assert_eq!(
            Registration::parse(quoted).unwrap(),
            Registration::parse(bare).unwrap()
        );
    }

    /// `InitOID_MessPtr` appends `:54925` when the host has no port, so a registration
    /// read back without one means the default and not a malformed field.
    #[test]
    fn a_host_without_a_port_means_the_listener_port() {
        let parsed = Registration::parse(
            "TYPE=BR;BUTTON=SCAN;USER=pc;FUNC=OCR;HOST=192.168.1.20;APPNUM=3;\
             DURATION=360;BRID=;",
        )
        .unwrap();
        assert_eq!(parsed.port, LISTENER_PORT);
        assert_eq!(parsed.host, Ipv4Addr::new(192, 168, 1, 20));
    }

    #[test]
    fn the_fifteen_alphanumeric_rule_is_enforced_rather_than_assumed() {
        assert!(UserName::new("desktop").is_ok());
        assert!(UserName::new("abcdefghijklmno").is_ok(), "15 is allowed");

        for (name, fragment) in [
            ("", "empty"),
            ("abcdefghijklmnop", "longer than 15"),
            ("jean.parpaillon", "other than ASCII"),
            ("my-desktop", "other than ASCII"),
            ("my desktop", "other than ASCII"),
            ("café", "other than ASCII"),
        ] {
            let error = UserName::new(name).unwrap_err();
            assert!(
                error.to_string().contains(fragment),
                "{name:?} was rejected with the wrong reason: {error}"
            );
        }
    }

    #[test]
    fn sanitising_keeps_the_alphanumerics_and_gives_up_when_there_are_none() {
        assert_eq!(
            UserName::sanitised("jean.parpaillon").unwrap().as_str(),
            "jeanparpaillon"
        );
        assert_eq!(
            UserName::sanitised("workstation-04.lan").unwrap().as_str(),
            "workstation04la",
            "the 15-character limit applies after filtering, not before"
        );
        assert_eq!(UserName::sanitised("...").map(|n| n.0), None);
        assert_eq!(UserName::sanitised("").map(|n| n.0), None);
    }

    #[test]
    fn a_missing_or_wrong_field_is_named_in_the_error() {
        assert_eq!(
            Registration::parse("BUTTON=SCAN;USER=pc;"),
            Err(RegisterError::MissingField("TYPE"))
        );
        assert_eq!(
            Registration::parse("TYPE=XX;USER=pc;FUNC=FILE;HOST=1.2.3.4;DURATION=1;"),
            Err(RegisterError::BadField {
                field: "TYPE",
                value: "XX".to_owned()
            })
        );
        assert_eq!(
            Registration::parse("TYPE=BR;USER=pc;FUNC=FAX;HOST=1.2.3.4;DURATION=1;"),
            Err(RegisterError::BadField {
                field: "FUNC",
                value: "FAX".to_owned()
            })
        );
        assert_eq!(
            Registration::parse("TYPE=BR;USER=pc;FUNC=FILE;HOST=not-an-ip;DURATION=1;"),
            Err(RegisterError::BadField {
                field: "HOST",
                value: "not-an-ip".to_owned()
            })
        );
        assert_eq!(
            Registration::parse("TYPE=BR;USER=pc;FUNC=FILE;HOST=1.2.3.4;DURATION=soon;"),
            Err(RegisterError::BadField {
                field: "DURATION",
                value: "soon".to_owned()
            })
        );
        // A registration whose USER the device would not have accepted is not a
        // registration, even though every field is present.
        assert!(matches!(
            Registration::parse("TYPE=BR;USER=jean.parpaillon;FUNC=FILE;HOST=1.2.3.4;DURATION=1;"),
            Err(RegisterError::UserName { .. })
        ));
    }

    #[test]
    fn a_lease_that_does_not_fit_the_field_is_refused_before_the_socket() {
        let mut registration = Registration::new(user("pc"), Ipv4Addr::LOCALHOST, Function::File);
        registration.duration = Duration::ZERO;
        assert_eq!(registration.value(), Err(RegisterError::Duration(0)));

        registration.duration = Duration::from_secs(u64::from(u32::MAX) + 1);
        assert!(matches!(
            registration.value(),
            Err(RegisterError::Duration(_))
        ));
    }

    #[test]
    fn the_set_request_carries_the_string_under_the_registration_oid() {
        let registration = Registration::new(
            user("desktop"),
            Ipv4Addr::new(192, 168, 1, 20),
            Function::Email,
        );
        let message = registration
            .set_request(snmp::DEFAULT_COMMUNITY, snmp::FIRST_REQUEST_ID)
            .unwrap();

        assert_eq!(message.pdu.kind, snmp::PduKind::SetRequest);
        assert_eq!(message.community, b"internal");
        assert_eq!(message.pdu.request_id, 200);
        let varbind = &message.pdu.varbinds[0];
        assert_eq!(varbind.oid.to_string(), REGISTRATION_OID);
        assert_eq!(
            varbind.value.as_str(),
            Some(registration.value().unwrap().as_str())
        );

        // …and it survives the trip through BER, which is where the 300-odd byte
        // registration string crosses the one-byte length boundary.
        let encoded = message.encode().unwrap();
        assert_eq!(snmp::Message::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn the_probe_is_a_read_of_the_same_oid() {
        let message = probe_request("public", 1);
        assert_eq!(message.pdu.kind, snmp::PduKind::GetRequest);
        assert_eq!(message.pdu.varbinds[0].oid.to_string(), REGISTRATION_OID);
        assert_eq!(
            message.pdu.varbinds[0].value,
            snmp::Value::OctetString(Vec::new())
        );
    }
}
