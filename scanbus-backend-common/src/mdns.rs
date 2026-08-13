//! One bounded mDNS browse window, shared by the backends that discover over DNS-SD.
//!
//! Every caller wants the same shape: start a browser, watch one or more service types
//! for a fixed window, keep whatever resolved, and never let an instance that does not
//! resolve decide when the window ends. The mobile backend browses
//! `_scanbus-mobile._tcp`; the hplip backend browses `_uscan._tcp` and `_scanner._tcp`
//! because `hp-probe --bus=net` only ever looks for a *printer*. Rather than a third
//! copy of the loop, the loop lives here.
//!
//! [`browse`] blocks — `mdns-sd` hands out a synchronous channel — so callers on an
//! async runtime must run it under `spawn_blocking`.

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
