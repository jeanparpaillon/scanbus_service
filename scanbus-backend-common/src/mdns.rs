//! Both directions of DNS-SD for a backend: one bounded browse window, and one record
//! this host publishes.
//!
//! Every browsing caller wants the same shape: start a browser, watch one or more
//! service types for a fixed window, keep whatever resolved, and never let an instance
//! that does not resolve decide when the window ends. The mobile backend browses
//! `_scanbus-mobile._tcp`; the hplip backend browses `_uscan._tcp` and `_scanner._tcp`
//! because `hp-probe --bus=net` only ever looks for a *printer*. Rather than a third
//! copy of the loop, the loop lives here.
//!
//! [`register`] is the opposite direction: this host announcing something a client
//! comes back to after its stored address stops working. It lives here for the same
//! reason the loop does — what a second copy would get wrong is not the record but the
//! `ServiceDaemon` lifetime around it, which has to outlive the registration and shut
//! down *after* the goodbye packet. [`Registration`] is that lifetime.
//!
//! [`browse`] blocks — `mdns-sd` hands out a synchronous channel — so callers on an
//! async runtime must run it under `spawn_blocking`. [`register`] does not block: the
//! responder answers queries on its own thread.

use std::fmt;
use std::time::{Duration, Instant};

use flume::select::SelectError;
use flume::{RecvError, Selector};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, warn};

/// Why a browse window never opened.
///
/// Nothing that happens *inside* the window is an error: an instance that never
/// resolves, a service type nobody answers and an empty network all yield an empty
/// result, because a backend must still be able to report the devices it found on its
/// other buses.
#[derive(Debug, thiserror::Error)]
pub enum BrowseError {
    /// No mDNS browser could be started at all — typically no usable interface.
    #[error("failed to start mDNS browser: {0}")]
    DaemonUnavailable(String),
    /// The browser started but refused every service type it was given.
    #[error("failed to browse {0}")]
    BrowseRefused(String),
}

/// Browse `service_types` for at most `timeout`, returning what resolved.
///
/// The types are a priority order, and the result keeps it: everything resolved on
/// `service_types[0]` comes first, then `service_types[1]`, and so on, each group in
/// the order the instances resolved. That is what lets the hplip backend prefer the
/// `_uscan._tcp` record for a device that also advertises `_scanner._tcp`, without
/// running — and waiting out — a second window for the secondary type.
///
/// Tolerated, deliberately: a service type the browser refuses (the others still run),
/// an instance that is announced but never resolves (it is simply absent), and a
/// browser that disconnects early (its type stops being watched, the others carry on).
/// The window is closed by `timeout` in every case; slow instances shorten nobody
/// else's share of it.
///
/// An instance that resolves more than once — `mdns-sd` re-emits as addresses arrive —
/// keeps its position in the order and its newest record, so the caller sees the most
/// complete address set rather than the first partial one.
pub fn browse(service_types: &[&str], timeout: Duration) -> Result<Vec<ServiceInfo>, BrowseError> {
    if service_types.is_empty() {
        return Ok(Vec::new());
    }

    let deadline = Instant::now() + timeout;
    let daemon =
        ServiceDaemon::new().map_err(|error| BrowseError::DaemonUnavailable(error.to_string()))?;

    let mut browses = Vec::with_capacity(service_types.len());
    let mut refusals = Vec::new();
    for service_type in service_types {
        match daemon.browse(service_type) {
            Ok(receiver) => browses.push((*service_type, receiver)),
            Err(error) => {
                warn!(%service_type, %error, "mDNS browse refused for this service type");
                refusals.push(format!("{service_type}: {error}"));
            }
        }
    }

    if browses.is_empty() {
        shutdown(&daemon);
        return Err(BrowseError::BrowseRefused(refusals.join("; ")));
    }

    // One bucket per browsed type: the concatenation at the end is what turns the
    // caller's argument order into a priority order.
    let mut buckets: Vec<Vec<ServiceInfo>> = vec![Vec::new(); browses.len()];
    let mut watching: Vec<usize> = (0..browses.len()).collect();

    while !watching.is_empty() && Instant::now() < deadline {
        // `wait_deadline` consumes the selector, so it is rebuilt each turn anyway;
        // that is also how a disconnected receiver drops out of the selection instead
        // of being polled forever.
        let mut selector = Selector::new();
        for &index in &watching {
            selector = selector.recv(&browses[index].1, move |event| (index, event));
        }

        match selector.wait_deadline(deadline) {
            Ok((index, Ok(ServiceEvent::ServiceResolved(service)))) => {
                record(&mut buckets[index], service);
            }
            Ok((_, Ok(_))) => {}
            Ok((index, Err(RecvError::Disconnected))) => {
                warn!(
                    service_type = %browses[index].0,
                    "mDNS browser disconnected before the window closed"
                );
                watching.retain(|&watched| watched != index);
            }
            Err(SelectError::Timeout) => break,
        }
    }

    for (service_type, _) in &browses {
        if let Err(error) = daemon.stop_browse(service_type) {
            debug!(%service_type, %error, "mDNS stop_browse failed; continuing");
        }
    }
    shutdown(&daemon);

    Ok(buckets.into_iter().flatten().collect())
}

/// How long `Drop` waits for the responder to confirm the goodbye it just sent.
///
/// The wait is on a local channel, not on the network: the daemon thread writes the
/// goodbye packet before it answers, so this is only ever long enough to notice a
/// responder that has already died.
const GOODBYE_TIMEOUT: Duration = Duration::from_millis(500);

/// Why a record was never published.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// No mDNS responder could be started at all — typically no usable interface.
    #[error("failed to start mDNS responder: {0}")]
    DaemonUnavailable(String),
    /// The instance, service type or TXT set is not something DNS-SD can carry.
    #[error("cannot publish this record: {0}")]
    Malformed(String),
    /// The responder started and then refused the record.
    #[error("failed to register {0}")]
    RegisterRefused(String),
}

/// A published record, alive for exactly as long as this guard.
///
/// Dropping it unregisters — which is what puts a goodbye packet on the wire, so
/// clients drop the instance immediately instead of waiting out its TTL — and then
/// shuts the responder down. Neither step can fail in a way the caller could act on at
/// that point, so both are logged at `debug` and the drop completes regardless.
pub struct Registration {
    daemon: ServiceDaemon,
    fullname: String,
}

// `ServiceDaemon` is a channel handle and is not `Debug`; the record's name is the only
// part of a registration worth printing anyway.
impl fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("fullname", &self.fullname)
            .finish_non_exhaustive()
    }
}

impl Registration {
    /// The published `<instance>.<service_type>`, as clients see it.
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        match self.daemon.unregister(&self.fullname) {
            // The responder handles commands in order, so the shutdown below cannot
            // overtake the goodbye; waiting on the confirmation is what also keeps the
            // process from exiting out from under it.
            Ok(confirmation) => {
                if let Err(error) = confirmation.recv_timeout(GOODBYE_TIMEOUT) {
                    debug!(
                        fullname = %self.fullname, %error,
                        "mDNS unregister was never confirmed; continuing"
                    );
                }
            }
            Err(error) => {
                debug!(fullname = %self.fullname, %error, "mDNS unregister failed; continuing");
            }
        }
        shutdown(&self.daemon);
    }
}

/// Publish `instance` of `service_type` on `port`, with `txt` as its TXT record.
///
/// `service_type` is the full DNS-SD type, `_something._tcp.local.` included; `txt` is
/// the key/value set verbatim, in order, with duplicate keys after the first dropped as
/// RFC 6763 §6.4 requires. Addresses are left to the responder — a host that gains or
/// loses an address while the guard is alive re-announces itself, which is the whole
/// point of publishing for a machine whose lease can change.
///
/// The record stays up until the returned [`Registration`] is dropped, so a caller that
/// drops it immediately has published nothing.
pub fn register(
    service_type: &str,
    instance: &str,
    port: u16,
    txt: &[(&str, &str)],
) -> Result<Registration, RegisterError> {
    let service = service_info(service_type, instance, port, txt)?;
    let fullname = service.get_fullname().to_owned();

    let daemon = ServiceDaemon::new()
        .map_err(|error| RegisterError::DaemonUnavailable(error.to_string()))?;

    if let Err(error) = daemon.register(service) {
        shutdown(&daemon);
        return Err(RegisterError::RegisterRefused(format!(
            "{fullname}: {error}"
        )));
    }

    debug!(%fullname, port, "published an mDNS record");
    Ok(Registration { daemon, fullname })
}

/// Describe the record, separately from publishing it, so the shape can be tested
/// without a responder and a network.
fn service_info(
    service_type: &str,
    instance: &str,
    port: u16,
    txt: &[(&str, &str)],
) -> Result<ServiceInfo, RegisterError> {
    ServiceInfo::new(
        service_type,
        instance,
        // The A/AAAA name, which `mdns-sd` requires fully qualified. Addresses stay
        // empty here and are filled in by `enable_addr_auto`.
        &format!("{instance}.local."),
        (),
        port,
        txt,
    )
    .map(ServiceInfo::enable_addr_auto)
    .map_err(|error| RegisterError::Malformed(format!("{instance}.{service_type}: {error}")))
}

/// Insert `service`, or replace the record already held for the same instance.
fn record(bucket: &mut Vec<ServiceInfo>, service: ServiceInfo) {
    match bucket
        .iter()
        .position(|seen| seen.get_fullname() == service.get_fullname())
    {
        Some(index) => {
            debug!(
                instance = %service.get_fullname(),
                "mDNS instance resolved again; keeping the newer record"
            );
            bucket[index] = service;
        }
        None => bucket.push(service),
    }
}

fn shutdown(daemon: &ServiceDaemon) {
    if let Err(error) = daemon.shutdown() {
        debug!(%error, "mDNS daemon shutdown failed; continuing");
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from(Ipv4Addr::new(10, 0, 0, last))
    }

    fn service(instance: &str, addresses: &[IpAddr]) -> ServiceInfo {
        ServiceInfo::new(
            "_uscan._tcp.local.",
            instance,
            &format!("{instance}.local."),
            addresses,
            8080,
            None,
        )
        .expect("test service info is well formed")
    }

    #[test]
    fn browse_without_service_types_starts_no_browser() {
        let started = Instant::now();
        let found = browse(&[], Duration::from_secs(30)).expect("empty browse is not an error");

        assert!(found.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an empty browse must not wait out its window"
        );
    }

    #[test]
    fn resolutions_keep_arrival_order() {
        let mut bucket = Vec::new();
        record(&mut bucket, service("first", &[ip(1)]));
        record(&mut bucket, service("second", &[ip(2)]));

        let instances: Vec<_> = bucket.iter().map(ServiceInfo::get_fullname).collect();
        assert_eq!(
            instances,
            ["first._uscan._tcp.local.", "second._uscan._tcp.local."]
        );
    }

    #[test]
    fn a_described_record_carries_the_instance_the_port_and_the_txt_verbatim() {
        let service = service_info(
            "_scanbus-host._tcp.local.",
            "workshop",
            45654,
            &[("id", "0123456789abcdef0123456789abcdef"), ("v", "1")],
        )
        .expect("a plain instance and two ASCII keys are publishable");

        assert_eq!(service.get_fullname(), "workshop._scanbus-host._tcp.local.");
        assert_eq!(service.get_hostname(), "workshop.local.");
        assert_eq!(service.get_port(), 45654);
        assert_eq!(
            service.get_property_val_str("id"),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(service.get_property_val_str("v"), Some("1"));
        assert_eq!(
            service.get_properties().len(),
            2,
            "TXT is exactly what the caller passed, with nothing added"
        );
        assert!(
            service.is_addr_auto(),
            "addresses must follow the host, or the record is stale the moment the lease changes"
        );
    }

    #[test]
    fn a_txt_key_dns_sd_cannot_carry_is_rejected_before_a_responder_starts() {
        let error = service_info(
            "_scanbus-host._tcp.local.",
            "workshop",
            45654,
            &[("id=nope", "1")],
        )
        .expect_err("'=' is not allowed in a TXT key");

        assert!(
            matches!(error, RegisterError::Malformed(_)),
            "a record that cannot exist is not a responder failure: {error}"
        );
    }

    #[test]
    fn re_resolution_replaces_the_record_in_place() {
        let mut bucket = Vec::new();
        record(&mut bucket, service("mfp", &[]));
        record(&mut bucket, service("other", &[ip(9)]));
        record(&mut bucket, service("mfp", &[ip(4)]));

        assert_eq!(bucket.len(), 2);
        assert_eq!(bucket[0].get_fullname(), "mfp._uscan._tcp.local.");
        assert_eq!(
            bucket[0].get_addresses().iter().collect::<Vec<_>>(),
            [&ip(4)]
        );
    }
}
