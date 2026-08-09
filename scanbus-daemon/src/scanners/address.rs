//! [`physical_address`]: the key two backends agree on when they found the same device.
//!
//! §9 says to deduplicate by physical address, and the reason it is not simply
//! `ScannerInfo::address` is that no two backends spell one device the same way. On this
//! machine, with `sane-airscan` 0.99.36 installed, one eSCL scanner is:
//!
//! ```text
//! sane   airscan:e0:Brother MFC-L2710DW      (and, for the same device over IP)
//! sane   escl:http://192.168.1.50:80/eSCL/
//! escl   http://192.168.1.50:80/eSCL/
//! ```
//!
//! What survives every one of those spellings is the endpoint: the host, or the USB
//! bus/device pair. So the key is the most physical thing that can be pulled out of the
//! address, and the full string only when nothing can be — two backends that report an
//! opaque address in different ways stay two objects, which is the safe direction to
//! fail in. Merging two devices into one object hides a scanner; failing to merge shows
//! a duplicate, which §3's `Backend` property already lets a client explain.
//!
//! Deliberately a heuristic in the daemon rather than a method on
//! [`ScannerInfo`](scanbus_core::ScannerInfo): the *backends* are what know how their
//! own addresses are built, and once two real ones exist (workstreams 5 and 6) the
//! honest fix is for each to report its own key. Until then this is one function with
//! its cases written down, not a rule scattered over the discovery path.

use scanbus_core::ScannerInfo;

/// The deduplication key for `info`, lowercased.
///
/// In order: a `usb:<bus>:<device>` triple, the host of a URL, a bare IPv4 address, or
/// the whole address string.
pub fn physical_address(info: &ScannerInfo) -> String {
    let address = info.address.trim().to_lowercase();

    usb_endpoint(&address)
        .or_else(|| url_host(&address))
        .or_else(|| ipv4(&address))
        .unwrap_or(address)
}

/// `usb:001:002` anywhere in the address, including SANE's `brother5:bus2;dev1` once it
/// has been normalised by a backend — only the plain form is recognised here.
fn usb_endpoint(address: &str) -> Option<String> {
    let start = address.find("usb:")?;
    let rest = &address[start + "usb:".len()..];

    let mut parts = rest.split(':');
    let bus = numeric_prefix(parts.next()?)?;
    let device = numeric_prefix(parts.next()?)?;

    Some(format!("usb:{bus}:{device}"))
}

/// The `host[:port]` of the first `scheme://…` in the address.
///
/// The port is kept: two services on one host are two devices as far as this daemon can
/// tell, and dropping it would merge a scanner with whatever else answers there.
fn url_host(address: &str) -> Option<String> {
    let start = address.find("://")?;
    let rest = &address[start + "://".len()..];
    let host = rest.split(['/', '?', '#']).next()?;

    (!host.is_empty()).then(|| host.trim_end_matches('.').to_owned())
}

/// A bare dotted-quad in the address, e.g. SANE's `epson2:net:192.168.1.50`.
fn ipv4(address: &str) -> Option<String> {
    address
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|token| {
            let octets: Vec<&str> = token.split('.').collect();
            octets.len() == 4
                && octets
                    .iter()
                    .all(|octet| !octet.is_empty() && octet.parse::<u8>().is_ok())
        })
        .map(str::to_owned)
}

/// The leading digits of `token`, if it starts with any.
fn numeric_prefix(token: &str) -> Option<&str> {
    let end = token
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(token.len());
    (end > 0).then(|| &token[..end])
}

#[cfg(test)]
mod tests {
    use scanbus_core::{Capabilities, ScannerId, Status};

    use super::*;

    fn scanner(backend: &str, address: &str) -> ScannerInfo {
        ScannerInfo {
            id: ScannerId::from_backend(backend, address).unwrap(),
            name: "a scanner".to_owned(),
            backend: backend.to_owned(),
            address: address.to_owned(),
            capabilities: Capabilities::default(),
            status: Status::Online,
        }
    }

    fn key(address: &str) -> String {
        physical_address(&scanner("sane", address))
    }

    /// The case this function exists for: one eSCL device, two backends, one key.
    #[test]
    fn sane_and_escl_agree_on_a_network_device() {
        assert_eq!(
            key("escl:http://192.168.1.50:80/eSCL/"),
            key("http://192.168.1.50:80/eSCL")
        );
        assert_eq!(key("http://192.168.1.50:80/eSCL"), "192.168.1.50:80");
    }

    #[test]
    fn a_sane_net_address_reduces_to_its_host() {
        assert_eq!(key("epson2:net:192.168.1.50"), "192.168.1.50");
        assert_eq!(key("EPSON2:NET:192.168.1.50"), "192.168.1.50");
    }

    #[test]
    fn a_usb_device_reduces_to_bus_and_device() {
        assert_eq!(key("usb:001:002"), "usb:001:002");
        assert_eq!(key("brother5:usb:001:002"), "usb:001:002");
        // Trailing detail after the device number is not part of the endpoint.
        assert_eq!(key("usb:001:002/scan"), "usb:001:002");
    }

    /// Two services on one host stay two devices: the port is part of the key.
    #[test]
    fn a_different_port_on_one_host_is_a_different_device() {
        assert_ne!(
            key("http://192.168.1.50:80/eSCL"),
            key("http://192.168.1.50:8080/eSCL")
        );
    }

    /// Nothing recognisable: the whole string, so two spellings stay two objects rather
    /// than one device being hidden behind the other.
    #[test]
    fn an_opaque_address_is_its_own_key() {
        assert_eq!(
            key("airscan:e0:Brother MFC-L2710DW"),
            "airscan:e0:brother mfc-l2710dw"
        );
        assert_ne!(
            key("airscan:e0:Brother MFC-L2710DW"),
            key("airscan:e1:Brother MFC-L2710DW")
        );
    }

    /// A hostname works as well as an address; mDNS names are what Avahi reports.
    #[test]
    fn a_hostname_is_a_key_too() {
        assert_eq!(
            key("http://BRW001122334455.local./eSCL"),
            "brw001122334455.local"
        );
    }

    /// Only the address matters — the id and the backend are exactly what differs
    /// between two sightings of one device.
    #[test]
    fn the_key_ignores_everything_but_the_address() {
        assert_eq!(
            physical_address(&scanner("sane", "epson2:net:192.168.1.50")),
            physical_address(&scanner("escl", "epson2:net:192.168.1.50"))
        );
    }

    #[test]
    fn a_version_like_token_is_not_mistaken_for_an_address() {
        assert_eq!(key("brother5:bus2;dev1"), "brother5:bus2;dev1");
        assert_eq!(key("0.99.36"), "0.99.36");
    }
}
