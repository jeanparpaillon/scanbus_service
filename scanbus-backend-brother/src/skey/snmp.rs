//! SNMPv1 over UDP/161, in the dialect `brscan-skey` speaks.
//!
//! Two PDU types, one transport, no MIB parsing and no v3 crypto. That is why there is
//! no SNMP crate here: the encoder and decoder below are smaller than the audit of a
//! dependency that would have to be pinned to get `SetRequest`, and every byte they emit
//! has to match a *specific* peer's expectations rather than the RFC's.
//!
//! # Where the dialect comes from
//!
//! Not from a packet capture. `/opt/brother/scanner/brscan-skey/brscan-skey-exe`
//! (0.3.4-0) ships **unstripped**, with a symbol table naming its translation units
//! (`snmp_encode.c`, `snmp_decode.c`, `registerpc.c`). Everything below is read off
//! `BerEncode1` at `0x406536` and its helpers, which is a stronger source than a capture:
//! a capture shows what one device was sent once, the encoder shows what is sent for
//! every input, including the length and integer boundaries a capture will never happen
//! to cross.
//!
//! Three places where the vendor departs from the RFC, all of which we reproduce because
//! the peer is the vendor's peer:
//!
//! - **The varbind value is always an OCTET STRING**, tag `0x04`, even in a `GetRequest`,
//!   where the RFC uses `05 00` (`NULL`). `BerEncNull` exists in the binary at `0x405da6`
//!   and is never called. A `GetRequest` therefore carries a zero-length octet string.
//!   [`Value::Null`] exists only so that a *response* using it decodes rather than
//!   failing — the device is not obliged to share the quirk.
//! - **Lengths are bounded by `u16`.** `BerEncLen` takes a `short`, so the encoder can
//!   emit at most `82 hi lo`. [`Message::encode`] refuses anything longer rather than
//!   silently emitting a form `brscan-skey` would never have produced.
//! - **The version field's encoded width is assumed to be 3 bytes** (`02 01 00`) when the
//!   outer length is computed. True for v1 and v2c; this module only offers v1.
//!
//! [`DEFAULT_COMMUNITY`] is likewise not a guess: `InitSnmpMess` at `0x407255` falls back
//! to the immediate `0x6c616e7265746e69` when `brscan-snmp.cfg` sets no `CommunityName=`,
//! and the file as shipped has that line commented out.

use std::fmt;
use std::str::FromStr;

/// Where SNMP lives. Named rather than inlined because the listener port (54925) is the
/// other number in this backend and confusing the two is a silent failure.
pub const SNMP_PORT: u16 = 161;

/// The community `brscan-skey` uses when its config names none — which is how it ships.
///
/// Note this is *not* `public`. Reads on the development machine's MFC-J5335DW answer on
/// both, but the vendor writes with `internal`, so that is what registration uses.
pub const DEFAULT_COMMUNITY: &str = "internal";

/// `InitSnmpMess` seeds the request-id counter with 200 and pre-increments it.
///
/// Kept only so that a request sequence starting from a cold daemon looks like the
/// vendor's. Nothing depends on the value; the id has to be echoed back, not predicted.
pub const FIRST_REQUEST_ID: i32 = 200;

/// Longest message this encoder will produce, from `BerEncLen`'s `short` parameter.
const MAX_ENCODED_LEN: usize = u16::MAX as usize;

// ------------------------------------------------------------------------ BER tags

const TAG_INTEGER: u8 = 0x02;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_NULL: u8 = 0x05;
const TAG_OID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;
/// PDU tags are context-specific constructed: `0xA0 | pdu_kind`.
const TAG_PDU_BASE: u8 = 0xA0;

// ------------------------------------------------------------------------- errors

/// Everything that can go wrong encoding or decoding a message.
///
/// Decoding runs on bytes from the network, so every variant here is a case that must
/// produce an `Err` rather than a panic or a truncated read — see
/// `every_prefix_of_a_valid_message_is_an_error_not_a_panic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnmpError {
    /// The input ended in the middle of a tag, a length, or a value.
    Truncated { needed: usize, available: usize },
    /// A tag was not the one the grammar requires at that position.
    UnexpectedTag { expected: u8, found: u8 },
    /// The PDU tag was not `0xA0..=0xA5`.
    UnknownPdu(u8),
    /// An indefinite length (`0x80`), or a long form wider than four bytes.
    UnsupportedLength(u8),
    /// A length field that claims more bytes than the enclosing value has.
    LengthOverrun { claimed: usize, available: usize },
    /// An INTEGER wider than four bytes, or of zero width.
    IntegerWidth(usize),
    /// A version field this module cannot have produced. Only v1 (`0`) exists here.
    UnsupportedVersion(i32),
    /// Fewer than two arcs, or a second arc a two-byte first octet cannot hold.
    BadOid(String),
    /// The message would exceed what `BerEncLen`'s `short` can express.
    TooLong(usize),
    /// Trailing bytes after a complete message.
    TrailingBytes(usize),
}

impl fmt::Display for SnmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, available } => write!(
                f,
                "truncated SNMP message: {needed} bytes needed, {available} available"
            ),
            Self::UnexpectedTag { expected, found } => {
                write!(f, "expected BER tag {expected:#04x}, found {found:#04x}")
            }
            Self::UnknownPdu(tag) => write!(f, "unknown SNMP PDU tag {tag:#04x}"),
            Self::UnsupportedLength(first) => {
                write!(f, "unsupported BER length form {first:#04x}")
            }
            Self::LengthOverrun { claimed, available } => write!(
                f,
                "BER length claims {claimed} bytes, {available} available"
            ),
            Self::IntegerWidth(width) => write!(f, "unsupported INTEGER width {width}"),
            Self::UnsupportedVersion(version) => {
                write!(
                    f,
                    "unsupported SNMP version field {version}; only v1 (0) is spoken"
                )
            }
            Self::BadOid(detail) => write!(f, "invalid object identifier: {detail}"),
            Self::TooLong(len) => write!(
                f,
                "encoded message is {len} bytes; brscan-skey's encoder cannot express \
                 more than {MAX_ENCODED_LEN}"
            ),
            Self::TrailingBytes(count) => {
                write!(f, "{count} bytes left over after the SNMP message")
            }
        }
    }
}

impl std::error::Error for SnmpError {}

// ---------------------------------------------------------------------------- OID

/// An object identifier, as a sequence of arcs.
///
/// Two arcs minimum, because the first encoded octet is `40 * arcs[0] + arcs[1]` and
/// `BerEncOid` returns an error below that — matching the vendor keeps a malformed OID
/// from being something only the device rejects.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Oid(Vec<u32>);

impl Oid {
    pub fn new(arcs: impl Into<Vec<u32>>) -> Result<Self, SnmpError> {
        let arcs = arcs.into();
        if arcs.len() < 2 {
            return Err(SnmpError::BadOid(format!(
                "at least two arcs are required, got {}",
                arcs.len()
            )));
        }
        // `BerEncOid` packs the first two arcs into one octet as `40 * a0 + a1` and
        // range-checks neither, so a `2.50` would encode to something that decodes back
        // as `3.10`. The constructor is where that stops: `a0 <= 2, a1 < 40` is exactly
        // the set the single-octet form can carry *and* recover. Every OID this backend
        // touches starts `1.3`, so this only ever fires on a caller's typo.
        if arcs[0] > 2 || arcs[1] >= 40 {
            return Err(SnmpError::BadOid(format!(
                "arcs {}.{} cannot be packed into one recoverable octet",
                arcs[0], arcs[1]
            )));
        }
        Ok(Self(arcs))
    }

    pub fn arcs(&self) -> &[u32] {
        &self.0
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        let mut content = Vec::with_capacity(self.0.len() + 4);
        content.push((40 * self.0[0] + self.0[1]) as u8);
        for arc in &self.0[2..] {
            encode_base128(*arc, &mut content);
        }
        out.push(TAG_OID);
        encode_len(content.len(), out);
        out.extend_from_slice(&content);
    }

    fn decode(content: &[u8]) -> Result<Self, SnmpError> {
        let (first, rest) = content
            .split_first()
            .ok_or_else(|| SnmpError::BadOid("empty OID content".to_owned()))?;
        let mut arcs = vec![u32::from(*first) / 40, u32::from(*first) % 40];

        let mut arc: u32 = 0;
        let mut in_progress = false;
        for byte in rest {
            // Seven bits at a time; five continuation bytes would overflow a u32, which
            // is the width `BerEncOid` works in.
            arc = arc
                .checked_mul(128)
                .and_then(|shifted| shifted.checked_add(u32::from(byte & 0x7f)))
                .ok_or_else(|| SnmpError::BadOid("arc does not fit in u32".to_owned()))?;
            if byte & 0x80 == 0 {
                arcs.push(arc);
                arc = 0;
                in_progress = false;
            } else {
                in_progress = true;
            }
        }
        if in_progress {
            return Err(SnmpError::BadOid(
                "OID ends in the middle of an arc".to_owned(),
            ));
        }
        Self::new(arcs)
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, arc) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(".")?;
            }
            write!(f, "{arc}")?;
        }
        Ok(())
    }
}

impl FromStr for Oid {
    type Err = SnmpError;

    fn from_str(dotted: &str) -> Result<Self, Self::Err> {
        let arcs = dotted
            .split('.')
            .map(|arc| {
                arc.parse::<u32>()
                    .map_err(|error| SnmpError::BadOid(format!("{arc:?}: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(arcs)
    }
}

// -------------------------------------------------------------------------- values

/// A varbind value.
///
/// [`Value::OctetString`] is the only one this backend ever *sends* — see the module
/// documentation on why even a `GetRequest` uses it. The other two exist because a
/// device's `Response` may use them and a decoder that failed on them would turn a
/// perfectly good answer into a protocol error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    OctetString(Vec<u8>),
    Integer(i32),
    Null,
}

impl Value {
    /// The value as text, when it is one. Registration answers are octet strings.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::OctetString(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::OctetString(bytes) => {
                out.push(TAG_OCTET_STRING);
                encode_len(bytes.len(), out);
                out.extend_from_slice(bytes);
            }
            Self::Integer(value) => encode_integer(TAG_INTEGER, *value, out),
            Self::Null => {
                out.push(TAG_NULL);
                out.push(0);
            }
        }
    }
}

/// One `(name, value)` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarBind {
    pub oid: Oid,
    pub value: Value,
}

impl VarBind {
    pub fn new(oid: Oid, value: Value) -> Self {
        Self { oid, value }
    }
}

// ----------------------------------------------------------------------------- PDU

/// The PDU types this module can name. The wire tag is `0xA0 | kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PduKind {
    GetRequest,
    GetNextRequest,
    Response,
    SetRequest,
}

impl PduKind {
    const fn code(self) -> u8 {
        match self {
            Self::GetRequest => 0,
            Self::GetNextRequest => 1,
            Self::Response => 2,
            Self::SetRequest => 3,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0xa0 => Some(Self::GetRequest),
            0xa1 => Some(Self::GetNextRequest),
            0xa2 => Some(Self::Response),
            0xa3 => Some(Self::SetRequest),
            _ => None,
        }
    }
}

/// `error-status`, kept as a named type because the whole point of a registration
/// `SetRequest` is to tell "the device took it" from "the device refused the OID".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorStatus {
    NoError,
    TooBig,
    NoSuchName,
    BadValue,
    ReadOnly,
    GenErr,
    Other(i32),
}

impl ErrorStatus {
    const fn code(self) -> i32 {
        match self {
            Self::NoError => 0,
            Self::TooBig => 1,
            Self::NoSuchName => 2,
            Self::BadValue => 3,
            Self::ReadOnly => 4,
            Self::GenErr => 5,
            Self::Other(code) => code,
        }
    }

    const fn from_code(code: i32) -> Self {
        match code {
            0 => Self::NoError,
            1 => Self::TooBig,
            2 => Self::NoSuchName,
            3 => Self::BadValue,
            4 => Self::ReadOnly,
            5 => Self::GenErr,
            other => Self::Other(other),
        }
    }

    pub const fn is_ok(self) -> bool {
        matches!(self, Self::NoError)
    }
}

impl fmt::Display for ErrorStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoError => f.write_str("noError"),
            Self::TooBig => f.write_str("tooBig"),
            Self::NoSuchName => f.write_str("noSuchName"),
            Self::BadValue => f.write_str("badValue"),
            Self::ReadOnly => f.write_str("readOnly"),
            Self::GenErr => f.write_str("genErr"),
            Self::Other(code) => write!(f, "error-status {code}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    pub kind: PduKind,
    pub request_id: i32,
    pub error_status: ErrorStatus,
    pub error_index: i32,
    pub varbinds: Vec<VarBind>,
}

// ------------------------------------------------------------------------- message

/// A whole SNMPv1 message. Only v1 exists here: it is what the vendor sends, and a
/// version field this module could not have produced is a decode error rather than a
/// silently accepted v2c reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V1,
}

impl Version {
    const fn code(self) -> i32 {
        match self {
            Self::V1 => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub version: Version,
    pub community: Vec<u8>,
    pub pdu: Pdu,
}

impl Message {
    /// A read of one OID. The value is an *empty octet string*, not `NULL` — see the
    /// module documentation.
    pub fn get(community: &str, request_id: i32, oid: Oid) -> Self {
        Self::request(PduKind::GetRequest, community, request_id, oid, Vec::new())
    }

    /// A write of one OID, which is what a registration is.
    pub fn set(community: &str, request_id: i32, oid: Oid, value: impl Into<Vec<u8>>) -> Self {
        Self::request(
            PduKind::SetRequest,
            community,
            request_id,
            oid,
            value.into(),
        )
    }

    fn request(kind: PduKind, community: &str, request_id: i32, oid: Oid, value: Vec<u8>) -> Self {
        Self {
            version: Version::V1,
            community: community.as_bytes().to_vec(),
            pdu: Pdu {
                kind,
                request_id,
                error_status: ErrorStatus::NoError,
                error_index: 0,
                varbinds: vec![VarBind::new(oid, Value::OctetString(value))],
            },
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, SnmpError> {
        let mut varbind_list = Vec::new();
        for varbind in &self.pdu.varbinds {
            let mut varbind_content = Vec::new();
            varbind.oid.encode_into(&mut varbind_content);
            varbind.value.encode_into(&mut varbind_content);
            push_constructed(TAG_SEQUENCE, &varbind_content, &mut varbind_list)?;
        }

        let mut pdu_content = Vec::new();
        encode_integer(TAG_INTEGER, self.pdu.request_id, &mut pdu_content);
        encode_integer(TAG_INTEGER, self.pdu.error_status.code(), &mut pdu_content);
        encode_integer(TAG_INTEGER, self.pdu.error_index, &mut pdu_content);
        push_constructed(TAG_SEQUENCE, &varbind_list, &mut pdu_content)?;

        let mut message_content = Vec::new();
        encode_integer(TAG_INTEGER, self.version.code(), &mut message_content);
        message_content.push(TAG_OCTET_STRING);
        encode_len(self.community.len(), &mut message_content);
        message_content.extend_from_slice(&self.community);
        push_constructed(
            TAG_PDU_BASE | self.pdu.kind.code(),
            &pdu_content,
            &mut message_content,
        )?;

        let mut out = Vec::new();
        push_constructed(TAG_SEQUENCE, &message_content, &mut out)?;
        if out.len() > MAX_ENCODED_LEN {
            return Err(SnmpError::TooLong(out.len()));
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SnmpError> {
        let mut reader = Reader::new(bytes);
        let body = reader.expect_tagged(TAG_SEQUENCE)?;
        // A datagram longer than the message it carries is not something to shrug at:
        // it means the framing disagrees with the sender's, and the next read would be
        // interpreting somebody else's bytes.
        if !reader.is_empty() {
            return Err(SnmpError::TrailingBytes(reader.remaining()));
        }

        let mut message = Reader::new(body);
        let version = match message.expect_integer()? {
            0 => Version::V1,
            other => return Err(SnmpError::UnsupportedVersion(other)),
        };
        let community = message.expect_tagged(TAG_OCTET_STRING)?.to_vec();

        let (pdu_tag, pdu_body) = message.expect_any()?;
        let kind = PduKind::from_tag(pdu_tag).ok_or(SnmpError::UnknownPdu(pdu_tag))?;

        let mut pdu = Reader::new(pdu_body);
        let request_id = pdu.expect_integer()?;
        let error_status = ErrorStatus::from_code(pdu.expect_integer()?);
        let error_index = pdu.expect_integer()?;

        let mut varbind_list = Reader::new(pdu.expect_tagged(TAG_SEQUENCE)?);
        let mut varbinds = Vec::new();
        while !varbind_list.is_empty() {
            let mut varbind = Reader::new(varbind_list.expect_tagged(TAG_SEQUENCE)?);
            let oid = Oid::decode(varbind.expect_tagged(TAG_OID)?)?;
            let (tag, content) = varbind.expect_any()?;
            let value = match tag {
                TAG_OCTET_STRING => Value::OctetString(content.to_vec()),
                TAG_INTEGER => Value::Integer(decode_integer(content)?),
                TAG_NULL => Value::Null,
                // A device may answer with an application type (Counter, TimeTicks…).
                // Keeping the bytes is more useful than refusing the whole message.
                _ => Value::OctetString(content.to_vec()),
            };
            varbinds.push(VarBind { oid, value });
        }

        Ok(Self {
            version,
            community,
            pdu: Pdu {
                kind,
                request_id,
                error_status,
                error_index,
                varbinds,
            },
        })
    }
}

// -------------------------------------------------------------------- BER encoding

/// `BerEncLen` at `0x405de9`: short form, then `81 xx`, then `82 hi lo`.
///
/// Deliberately not the general long form. The vendor's parameter is a `short`, so
/// `83`/`84` are lengths `brscan-skey` cannot produce; emitting one would be a difference
/// from the reference implementation hidden inside a case no test happens to reach.
fn encode_len(len: usize, out: &mut Vec<u8>) {
    if len <= 0x7f {
        out.push(len as u8);
    } else if len <= 0xff {
        out.push(0x81);
        out.push(len as u8);
    } else {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push(len as u8);
    }
}

fn push_constructed(tag: u8, content: &[u8], out: &mut Vec<u8>) -> Result<(), SnmpError> {
    if content.len() > MAX_ENCODED_LEN {
        return Err(SnmpError::TooLong(content.len()));
    }
    out.push(tag);
    encode_len(content.len(), out);
    out.extend_from_slice(content);
    Ok(())
}

/// `BerEncInteger1` at `0x406216`: minimal two's complement, one to four octets.
fn encode_integer(tag: u8, value: i32, out: &mut Vec<u8>) {
    let width = if (-0x80..0x80).contains(&value) {
        1
    } else if (-0x8000..0x8000).contains(&value) {
        2
    } else if (-0x0080_0000..0x0080_0000).contains(&value) {
        3
    } else {
        4
    };
    out.push(tag);
    out.push(width as u8);
    for shift in (0..width).rev() {
        out.push((value >> (8 * shift)) as u8);
    }
}

fn encode_base128(mut arc: u32, out: &mut Vec<u8>) {
    let mut group = [0u8; 5];
    let mut count = 0;
    loop {
        group[count] = (arc & 0x7f) as u8;
        count += 1;
        arc >>= 7;
        if arc == 0 {
            break;
        }
    }
    for index in (0..count).rev() {
        let last = index == 0;
        out.push(if last {
            group[index]
        } else {
            group[index] | 0x80
        });
    }
}

fn decode_integer(content: &[u8]) -> Result<i32, SnmpError> {
    if content.is_empty() || content.len() > 4 {
        return Err(SnmpError::IntegerWidth(content.len()));
    }
    let mut value = i32::from(content[0] as i8);
    for byte in &content[1..] {
        value = (value << 8) | i32::from(*byte);
    }
    Ok(value)
}

// -------------------------------------------------------------------- BER decoding

/// A cursor over one BER value's content.
///
/// Every method returns a borrowed slice rather than copying, and every one of them
/// checks its bounds before slicing: this reads datagrams off the network, so "the length
/// byte said 200" must become an `Err`, never a panic.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn remaining(&self) -> usize {
        self.bytes.len()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], SnmpError> {
        if self.bytes.len() < count {
            return Err(SnmpError::Truncated {
                needed: count,
                available: self.bytes.len(),
            });
        }
        let (head, tail) = self.bytes.split_at(count);
        self.bytes = tail;
        Ok(head)
    }

    fn take_byte(&mut self) -> Result<u8, SnmpError> {
        Ok(self.take(1)?[0])
    }

    fn read_len(&mut self) -> Result<usize, SnmpError> {
        let first = self.take_byte()?;
        if first & 0x80 == 0 {
            return Ok(usize::from(first));
        }
        let width = usize::from(first & 0x7f);
        // 0x80 is the indefinite form, which needs an end-of-contents sentinel this
        // grammar has no place for; beyond four octets the length cannot address a
        // datagram anyway.
        if width == 0 || width > 4 {
            return Err(SnmpError::UnsupportedLength(first));
        }
        let mut len = 0usize;
        for byte in self.take(width)? {
            len = (len << 8) | usize::from(*byte);
        }
        Ok(len)
    }

    /// Tag, then length, then that many bytes.
    fn expect_any(&mut self) -> Result<(u8, &'a [u8]), SnmpError> {
        let tag = self.take_byte()?;
        let len = self.read_len()?;
        if len > self.bytes.len() {
            return Err(SnmpError::LengthOverrun {
                claimed: len,
                available: self.bytes.len(),
            });
        }
        Ok((tag, self.take(len)?))
    }

    fn expect_tagged(&mut self, expected: u8) -> Result<&'a [u8], SnmpError> {
        let (tag, content) = self.expect_any()?;
        if tag != expected {
            return Err(SnmpError::UnexpectedTag {
                expected,
                found: tag,
            });
        }
        Ok(content)
    }

    fn expect_integer(&mut self) -> Result<i32, SnmpError> {
        decode_integer(self.expect_tagged(TAG_INTEGER)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(dotted: &str) -> Oid {
        dotted.parse().unwrap()
    }

    /// The whole message, byte for byte, against what `BerEncode1` would have written.
    ///
    /// Hand-assembled from the disassembly rather than from this encoder, so it fails if
    /// the encoder is changed to something merely self-consistent.
    #[test]
    fn a_short_set_request_encodes_exactly_as_the_vendor_encoder_would() {
        let message = Message::set("x", 1, oid("1.3"), Vec::new());
        #[rustfmt::skip]
        let expected = vec![
            0x30, 0x1a,             // SEQUENCE, 26 bytes of content
            0x02, 0x01, 0x00,       //   version: SNMPv1
            0x04, 0x01, b'x',       //   community "x"
            0xa3, 0x12,             //   SetRequest (0xA0 | 3), 18 bytes
            0x02, 0x01, 0x01,       //     request-id 1
            0x02, 0x01, 0x00,       //     error-status noError
            0x02, 0x01, 0x00,       //     error-index 0
            0x30, 0x07,             //     varbind list
            0x30, 0x05,             //       varbind
            0x06, 0x01, 0x2b,       //         OID 1.3, packed as 40*1 + 3 = 0x2b
            0x04, 0x00,             //         value: empty OCTET STRING, never NULL
        ];
        assert_eq!(message.encode().unwrap(), expected);
        assert_eq!(Message::decode(&expected).unwrap(), message);
    }

    #[test]
    fn a_get_request_carries_an_empty_octet_string_not_null() {
        let encoded = Message::get("public", 7, oid("1.3.6.1.2.1.1.1.0"))
            .encode()
            .unwrap();
        // The last two bytes are the value. `05 00` would be the RFC's NULL; the vendor
        // emits `04 00`, and BerEncNull is dead code in the binary.
        assert_eq!(&encoded[encoded.len() - 2..], &[TAG_OCTET_STRING, 0x00]);
        assert_eq!(encoded[0], TAG_SEQUENCE);
        assert!(encoded.contains(&0xa0), "GetRequest tag is 0xA0");
    }

    #[test]
    fn the_registration_oid_round_trips_through_its_multi_byte_arc() {
        // 2435 = 19 * 128 + 3, so it needs two base-128 octets — the one arc in this
        // backend's only OID that a single-octet encoder would silently mangle.
        let registration = oid("1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0");
        let mut encoded = Vec::new();
        registration.encode_into(&mut encoded);
        assert_eq!(
            encoded,
            vec![
                TAG_OID, 0x0f, // OID, 15 content octets
                0x2b, // 1.3 packed as 40*1 + 3
                0x06, 0x01, 0x04, 0x01, //
                0x93, 0x03, // 2435, base-128 over two octets
                0x02, 0x03, 0x09, 0x02, 0x0b, 0x01, 0x01, 0x00,
            ]
        );
        assert_eq!(Oid::decode(&encoded[2..]).unwrap(), registration);
        assert_eq!(
            registration.to_string(),
            "1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0"
        );
    }

    #[test]
    fn oids_need_two_arcs_and_a_packable_second() {
        assert!(matches!(Oid::new(vec![1]), Err(SnmpError::BadOid(_))));
        assert!(matches!(Oid::new(vec![1, 99]), Err(SnmpError::BadOid(_))));
        assert!(Oid::new(vec![1, 3]).is_ok());
        assert!(matches!("1.3.x".parse::<Oid>(), Err(SnmpError::BadOid(_))));
    }

    #[test]
    fn every_length_form_the_vendor_can_emit_round_trips() {
        // 127/128 is the short/`81` boundary and 255/256 the `81`/`82` one. A payload
        // that never crosses them is exactly the payload a single capture would show.
        for len in [0usize, 1, 126, 127, 128, 254, 255, 256, 257, 4096] {
            let payload = vec![b'v'; len];
            let message = Message::set(
                DEFAULT_COMMUNITY,
                FIRST_REQUEST_ID,
                oid("1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0"),
                payload.clone(),
            );
            let encoded = message.encode().unwrap();
            let decoded = Message::decode(&encoded).unwrap();
            assert_eq!(decoded, message, "length {len} did not round-trip");
            assert_eq!(
                decoded.pdu.varbinds[0].value,
                Value::OctetString(payload),
                "length {len}"
            );
        }
    }

    #[test]
    fn lengths_use_the_narrowest_form() {
        let mut out = Vec::new();
        encode_len(0x7f, &mut out);
        assert_eq!(out, vec![0x7f]);

        out.clear();
        encode_len(0x80, &mut out);
        assert_eq!(out, vec![0x81, 0x80]);

        out.clear();
        encode_len(0xff, &mut out);
        assert_eq!(out, vec![0x81, 0xff]);

        out.clear();
        encode_len(0x0100, &mut out);
        assert_eq!(out, vec![0x82, 0x01, 0x00]);
    }

    #[test]
    fn integers_are_minimal_two_s_complement_at_every_width_boundary() {
        let cases: &[(i32, &[u8])] = &[
            (0, &[0x02, 0x01, 0x00]),
            (1, &[0x02, 0x01, 0x01]),
            (127, &[0x02, 0x01, 0x7f]),
            (128, &[0x02, 0x02, 0x00, 0x80]),
            (255, &[0x02, 0x02, 0x00, 0xff]),
            (32767, &[0x02, 0x02, 0x7f, 0xff]),
            (32768, &[0x02, 0x03, 0x00, 0x80, 0x00]),
            (-1, &[0x02, 0x01, 0xff]),
            (-128, &[0x02, 0x01, 0x80]),
            (-129, &[0x02, 0x02, 0xff, 0x7f]),
            (i32::MAX, &[0x02, 0x04, 0x7f, 0xff, 0xff, 0xff]),
            (i32::MIN, &[0x02, 0x04, 0x80, 0x00, 0x00, 0x00]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            encode_integer(TAG_INTEGER, *value, &mut out);
            assert_eq!(&out, expected, "encoding {value}");
            assert_eq!(
                decode_integer(&out[2..]).unwrap(),
                *value,
                "decoding {value}"
            );
        }
    }

    #[test]
    fn a_response_decodes_with_its_error_status_and_value() {
        // What a `noSuchName` refusal of the registration OID looks like coming back.
        let refusal = Message {
            version: Version::V1,
            community: DEFAULT_COMMUNITY.as_bytes().to_vec(),
            pdu: Pdu {
                kind: PduKind::Response,
                request_id: 201,
                error_status: ErrorStatus::NoSuchName,
                error_index: 1,
                varbinds: vec![VarBind::new(
                    oid("1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0"),
                    Value::OctetString(b"TRUE".to_vec()),
                )],
            },
        };
        let decoded = Message::decode(&refusal.encode().unwrap()).unwrap();
        assert_eq!(decoded, refusal);
        assert!(!decoded.pdu.error_status.is_ok());
        assert_eq!(decoded.pdu.varbinds[0].value.as_str(), Some("TRUE"));
        assert_eq!(decoded.pdu.error_status.to_string(), "noSuchName");
    }

    #[test]
    fn a_null_or_integer_valued_response_decodes_rather_than_failing() {
        // We never send these; a device is under no obligation to share our quirk.
        for value in [Value::Null, Value::Integer(-7)] {
            let message = Message {
                version: Version::V1,
                community: b"public".to_vec(),
                pdu: Pdu {
                    kind: PduKind::Response,
                    request_id: 1,
                    error_status: ErrorStatus::NoError,
                    error_index: 0,
                    varbinds: vec![VarBind::new(oid("1.3.6.1.2.1.1.1.0"), value.clone())],
                },
            };
            let decoded = Message::decode(&message.encode().unwrap()).unwrap();
            assert_eq!(decoded.pdu.varbinds[0].value, value);
        }
    }

    #[test]
    fn a_multi_varbind_response_keeps_every_pair_in_order() {
        let message = Message {
            version: Version::V1,
            community: b"internal".to_vec(),
            pdu: Pdu {
                kind: PduKind::Response,
                request_id: 12345,
                error_status: ErrorStatus::NoError,
                error_index: 0,
                varbinds: vec![
                    VarBind::new(oid("1.3.6.1.2.1.1.1.0"), Value::OctetString(b"a".to_vec())),
                    VarBind::new(
                        oid("1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0"),
                        Value::OctetString(vec![b'b'; 300]),
                    ),
                ],
            },
        };
        assert_eq!(
            Message::decode(&message.encode().unwrap()).unwrap(),
            message
        );
    }

    /// The property that matters for a socket: no input is a panic.
    ///
    /// Every truncation of a valid message is exercised, not a sampled few — a decoder
    /// that reads one byte past the end does it at exactly one length.
    #[test]
    fn every_prefix_of_a_valid_message_is_an_error_not_a_panic() {
        let encoded = Message::set(
            DEFAULT_COMMUNITY,
            FIRST_REQUEST_ID,
            oid("1.3.6.1.4.1.2435.2.3.9.2.11.1.1.0"),
            vec![b'x'; 300],
        )
        .encode()
        .unwrap();

        for len in 0..encoded.len() {
            assert!(
                Message::decode(&encoded[..len]).is_err(),
                "a {len}-byte prefix decoded as a whole message"
            );
        }
        assert!(Message::decode(&encoded).is_ok());
    }

    #[test]
    fn garbage_and_hostile_lengths_are_errors() {
        assert!(matches!(
            Message::decode(&[]),
            Err(SnmpError::Truncated { .. })
        ));
        // Indefinite length: legal BER, no end-of-contents in this grammar.
        assert!(matches!(
            Message::decode(&[0x30, 0x80, 0x02, 0x01, 0x00]),
            Err(SnmpError::UnsupportedLength(0x80))
        ));
        // A length that claims far more than the datagram holds.
        assert!(matches!(
            Message::decode(&[0x30, 0x82, 0xff, 0xff, 0x00]),
            Err(SnmpError::LengthOverrun { .. })
        ));
        // Right shape, wrong PDU tag.
        let mut wrong_pdu = Message::get("public", 1, oid("1.3")).encode().unwrap();
        let pdu_index = wrong_pdu.iter().position(|byte| *byte == 0xa0).unwrap();
        wrong_pdu[pdu_index] = 0xa7;
        assert!(matches!(
            Message::decode(&wrong_pdu),
            Err(SnmpError::UnknownPdu(0xa7))
        ));
        // A whole valid message with a byte glued on the end.
        let mut trailing = Message::get("public", 1, oid("1.3")).encode().unwrap();
        trailing.push(0x00);
        assert!(matches!(
            Message::decode(&trailing),
            Err(SnmpError::TrailingBytes(1))
        ));
    }

    #[test]
    fn every_single_byte_corruption_decodes_or_errors_but_never_panics() {
        let encoded = Message::set(DEFAULT_COMMUNITY, 200, oid("1.3.6.1.4.1.2435.2"), b"v")
            .encode()
            .unwrap();
        for index in 0..encoded.len() {
            for replacement in [0x00u8, 0x01, 0x7f, 0x80, 0xa0, 0xff] {
                let mut corrupt = encoded.clone();
                corrupt[index] = replacement;
                let _ = Message::decode(&corrupt);
            }
        }
    }
}
