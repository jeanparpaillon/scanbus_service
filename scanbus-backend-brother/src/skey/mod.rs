//! The `brscan-skey` protocol, without `brscan-skey`.
//!
//! Two things the vendor daemon does that scanbus needs — an SNMP registration on
//! UDP/161 and a listener on UDP/54925 — implemented here as **parsers and builders with
//! no socket in sight**. That order is deliberate and is the same one the mobile backend
//! used: the protocol is the part that can be wrong, and a printer is the slowest
//! possible place to find that out. Nothing in this module opens a file descriptor;
//! `cargo test -p scanbus-backend-brother` needs no hardware and no network.
//!
//! # Where this comes from
//!
//! Not from a packet capture. `/opt/brother/scanner/brscan-skey/brscan-skey-exe`
//! (`brscan-skey` 0.3.4-0) ships **unstripped** — the symbol table still names its
//! translation units, `snmp_encode.c`, `snmp_decode.c`, `udp_agent.c`, `registerpc.c`,
//! `scramble.c`. Every constant and every framing rule below is cited to the function it
//! was read out of, so the claim is checkable rather than remembered:
//!
//! | What | Where |
//! | --- | --- |
//! | SNMP message layout, PDU tag `0xA0 \| op` | `BerEncode1` `0x406536` |
//! | Length forms, integer widths, OID packing | `BerEncLen` `0x405de9`, `BerEncInteger1` `0x406216`, `BerEncOid` `0x405e91` |
//! | Community default, version, first request-id | `InitSnmpMess` `0x407255` |
//! | Registration string, `HOST` port default | `InitOID_MessPtr` `sprintf` at `0x406efe`, format at `0x414a78` |
//! | Datagram framing | `udp_sent` `0x40884f` |
//! | Frame dispatch and validation | `check_udp_data` `0x408134` |
//! | Key-press fields | `decode_key_data` `0x40bad1` |
//!
//! A capture shows what one device was sent once. The encoder shows what is sent for
//! every input, including the length and integer boundaries a capture will never happen
//! to cross — which is why the round-trip tests here exercise 127/128 and 255/256 rather
//! than one recorded payload.
//!
//! # What is still unknown
//!
//! - Whether a `SetRequest` on the registration OID is **accepted** by the MFC-J5335DW.
//!   The OID exists and reads back `TRUE`; that a write takes is a different claim and
//!   this module cannot make it.
//! - Whether that firmware is a "Phoenix" one, which takes the same registration string
//!   over HTTPS (`POST /phoenix/mib`, digest auth `Public:0000`) instead of SNMP.
//!   `ISPhoenixFirmware` decides at runtime in the vendor daemon; scanbus implements only
//!   the SNMP path so far.
//! - The `(id, code)` a real device puts in front of a key press. `check_udp_data`
//!   requires `0x01`/`0x01` and [`event`] enforces exactly that, so a device that used
//!   another pair would surface as [`event::EventError::UnknownFrame`] naming the bytes,
//!   rather than as silence.

pub mod event;
pub mod register;
pub mod snmp;

mod fields;
