//! `list`, `show` and `discover` against a bus with a full `Scanner1` on it
//! ([`scanbus-cli.md`] §3–§4, issue [8.5]).
//!
//! Assertions are on `--json` and on exit codes, never on the human table — same
//! convention as `select.rs` and `status.rs`, and for the same reason (§10): the table's
//! layout is free to change.
//!
//! [8.5]: https://github.com/jeanparpaillon/scanbus_service/issues/32
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{PrivateBus, skipped};
use zbus::zvariant::OwnedValue;

const PAIRED: &str = "brother_net_192_2E168_2E1_2E23";
const UNPAIRED: &str = "escl_avahi_HP_OfficeJet_8010";
/// Exported once `StartDiscovery` is called, to exercise `discover`'s stream.
const DISCOVERED: &str = "sane_usb_0403_6083";

/// A full `org.scanbus.Scanner1`, every property §3 documents.
struct Scanner {
    id: &'static str,
    name: &'static str,
    paired: bool,
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl Scanner {
    #[zbus(property)]
    fn id(&self) -> String {
        self.id.to_owned()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.name.to_owned()
    }

    #[zbus(property)]
    fn backend(&self) -> String {
        "mock".to_owned()
    }

    #[zbus(property)]
    fn address(&self) -> String {
        "mock://".to_owned()
    }

    #[zbus(property)]
    fn capabilities(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    #[zbus(property)]
    fn supported_profiles(&self) -> Vec<String> {
        vec!["document".to_owned()]
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        self.paired
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "online".to_owned()
    }

    #[zbus(property)]
    fn default_profile(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn pairing_state(&self) -> String {
        "none".to_owned()
    }

    #[zbus(property)]
    fn pairing_error(&self) -> String {
        String::new()
    }
}

/// A `Button1` of the paired scanner, for `show`'s button table.
struct Button {
    index: u32,
    device_label: &'static str,
}

#[zbus::interface(name = "org.scanbus.Button1")]
impl Button {
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.index
    }

    #[zbus(property)]
    fn device_label(&self) -> String {
        self.device_label.to_owned()
    }
}

/// A `Manager1` whose `StartDiscovery` exports one more scanner shortly afterwards, and
/// whose `StopDiscovery` is observable.
struct Manager {
    stops: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    async fn start_discovery(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        _filters: HashMap<String, OwnedValue>,
    ) {
        let server = server.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = server
                .at(
                    format!("/org/scanbus/scanner/{DISCOVERED}"),
                    Scanner {
                        id: DISCOVERED,
                        name: "Canon LiDE",
                        paired: false,
                    },
                )
                .await;
        });
    }

    fn stop_discovery(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }

    fn get_profile_types(&self) -> Vec<String> {
        vec!["document".to_owned()]
    }
}

/// Owns `org.scanbus` on `address` until the returned connection is dropped: one paired
/// scanner with a one-key menu, optionally one already-unpaired scanner, and a
/// `Manager1` that answers `discover`.
async fn serve(address: &str, stops: Arc<AtomicUsize>, with_unpaired: bool) -> zbus::Connection {
    let connection = zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager { stops })
        .expect("/org/scanbus is a valid path")
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is a valid path")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus");

    let server = connection.object_server();

    server
        .at(
            format!("/org/scanbus/scanner/{PAIRED}"),
            Scanner {
                id: PAIRED,
                name: "MFC-L2710DW",
                paired: true,
            },
        )
        .await
        .expect("cannot export the paired scanner");
    server
        .at(
            format!("/org/scanbus/scanner/{PAIRED}/button/0"),
            Button {
                index: 0,
                device_label: "Scan to File",
            },
        )
        .await
        .expect("cannot export the button");

    if with_unpaired {
        server
            .at(
                format!("/org/scanbus/scanner/{UNPAIRED}"),
                Scanner {
                    id: UNPAIRED,
                    name: "HP OfficeJet 8010",
                    paired: false,
                },
            )
            .await
            .expect("cannot export the unpaired scanner");
    }

    connection
}

/// A bus with the fixture tree on it, or `None` when `dbus-daemon` is missing.
async fn fixture() -> Option<(PrivateBus, zbus::Connection, Arc<AtomicUsize>)> {
    let bus = PrivateBus::start()?;
    let stops = Arc::new(AtomicUsize::new(0));
    let daemon = serve(bus.address(), Arc::clone(&stops), true).await;
    Some((bus, daemon, stops))
}

/// A bus with only the paired scanner on it — no unpaired scanner exists yet, so
/// `discover` has no reason to think another session is already running.
async fn fixture_without_unpaired() -> Option<(PrivateBus, zbus::Connection, Arc<AtomicUsize>)> {
    let bus = PrivateBus::start()?;
    let stops = Arc::new(AtomicUsize::new(0));
    let daemon = serve(bus.address(), Arc::clone(&stops), false).await;
    Some((bus, daemon, stops))
}

/// Acceptance: `list` reports every scanner `GetManagedObjects` holds, with the D-Bus
/// property names verbatim, and starts no discovery.
#[tokio::test(flavor = "multi_thread")]
async fn list_reports_what_the_daemon_already_knows() {
    let Some((bus, _daemon, stops)) = fixture().await else {
        return skipped("list_reports_what_the_daemon_already_knows");
    };

    let scanners = bus.scanbus(&["--json", "list"]).assert_code(0).json();
    let ids: Vec<&str> = scanners
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["Id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&PAIRED), "{ids:?}");
    assert!(ids.contains(&UNPAIRED), "{ids:?}");

    let paired_only = bus
        .scanbus(&["--json", "list", "--paired"])
        .assert_code(0)
        .json();
    assert_eq!(paired_only.as_array().unwrap().len(), 1);
    assert_eq!(paired_only[0]["Id"], PAIRED);
    assert_eq!(paired_only[0]["Paired"], true);

    let unpaired_only = bus
        .scanbus(&["--json", "list", "--unpaired"])
        .assert_code(0)
        .json();
    assert_eq!(unpaired_only[0]["Id"], UNPAIRED);
    assert_eq!(unpaired_only[0]["Paired"], false);

    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "list must never touch discovery"
    );
}

/// Acceptance: `show` reports every `Scanner1` property plus the scanner's buttons.
#[tokio::test(flavor = "multi_thread")]
async fn show_reports_properties_and_buttons() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("show_reports_properties_and_buttons");
    };

    let document = bus
        .scanbus(&["--json", "show", PAIRED])
        .assert_code(0)
        .json();

    assert_eq!(document["Id"], PAIRED);
    assert_eq!(document["Name"], "MFC-L2710DW");
    assert_eq!(document["Paired"], true);
    assert_eq!(document["SupportedProfiles"][0], "document");

    let buttons = document["Buttons"].as_array().expect("Buttons is an array");
    assert_eq!(buttons.len(), 1);
    assert_eq!(buttons[0]["Index"], 0);
    assert_eq!(buttons[0]["DeviceLabel"], "Scan to File");
}

/// Acceptance: `discover --for` streams the scanner that arrives while the session
/// runs, as JSON Lines, then stops the session it started because no unpaired scanner
/// existed beforehand.
#[tokio::test(flavor = "multi_thread")]
async fn discover_streams_arrivals_and_stops_its_own_session() {
    let Some((bus, _daemon, stops)) = fixture_without_unpaired().await else {
        return skipped("discover_streams_arrivals_and_stops_its_own_session");
    };

    let run = bus.scanbus(&["--json", "discover", "--for", "300ms"]);
    run.assert_code(0);

    let mut ids = Vec::new();
    for line in run.stdout.lines() {
        let event: serde_json::Value = serde_json::from_str(line).expect(line);
        assert_eq!(event["event"], "added");
        ids.push(
            event["interfaces"]["org.scanbus.Scanner1"]["Id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }

    assert!(
        ids.contains(&DISCOVERED.to_owned()),
        "the scanner that arrived mid-session must be streamed: {ids:?}"
    );

    // No unpaired scanner existed before this call, so this process is the one that
    // started the session and must be the one that stops it (§7's fallback guess).
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "discover must release the session it believes it started"
    );
}

/// Acceptance: an unpaired scanner already visible before `StartDiscovery` looks like
/// someone else's session already running, so `discover` leaves it be on exit.
#[tokio::test(flavor = "multi_thread")]
async fn an_already_unpaired_scanner_stops_discover_from_ending_the_session() {
    let Some((bus, _daemon, stops)) = fixture().await else {
        return skipped("an_already_unpaired_scanner_stops_discover_from_ending_the_session");
    };

    let run = bus.scanbus(&["discover", "--for", "100ms"]);
    run.assert_code(0);

    assert!(
        run.stderr.contains("leaving discovery running"),
        "{}",
        run.stderr
    );
    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "an unpaired scanner already existed, so this process must not guess it owns the session"
    );
}

/// Acceptance: `--keep` leaves the session running.
#[tokio::test(flavor = "multi_thread")]
async fn keep_leaves_the_session_running() {
    let Some((bus, _daemon, stops)) = fixture_without_unpaired().await else {
        return skipped("keep_leaves_the_session_running");
    };

    bus.scanbus(&["discover", "--for", "100ms", "--keep"])
        .assert_code(0);

    assert_eq!(stops.load(Ordering::SeqCst), 0, "--keep must not stop it");
}
