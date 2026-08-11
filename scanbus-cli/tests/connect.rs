//! `connect`, `disconnect` and `scan` against a private bus with a stand-in `Scanner1`
//! ([8.7]).

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{PrivateBus, skipped};
use zbus::DBusError;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

const ONLINE: &str = "brother_net_192_2E168_2E1_2E23";
const OFFLINE: &str = "brother_net_192_2E168_2E1_2E24";
const UNPAIRED: &str = "escl_avahi_HP_OfficeJet_8010";
const JOB_ID: u64 = 7;
const JOB_PATH: &str = "/org/scanbus/scanner/brother_net_192_2E168_2E1_2E23/job/7";

#[derive(Debug, DBusError)]
#[zbus(prefix = "org")]
enum MockError {
    #[zbus(name = "scanbus.Error.NotPaired")]
    NotPaired(String),
    #[zbus(name = "scanbus.Error.NotReachable")]
    NotReachable(String),
}

struct Manager;

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    fn get_profile_types(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
    }
}

#[derive(Clone)]
struct Counts {
    connect: Arc<AtomicUsize>,
    disconnect: Arc<AtomicUsize>,
    scan: Arc<AtomicUsize>,
}

impl Counts {
    fn new() -> Self {
        Self {
            connect: Arc::new(AtomicUsize::new(0)),
            disconnect: Arc::new(AtomicUsize::new(0)),
            scan: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct ScannerWithScan {
    id: &'static str,
    name: &'static str,
    paired: bool,
    status: &'static str,
    counts: Counts,
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl ScannerWithScan {
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
        "mock://scanner".to_owned()
    }

    #[zbus(property)]
    fn capabilities(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    #[zbus(property)]
    fn supported_profiles(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
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
        self.status.to_owned()
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

    #[zbus(property)]
    fn pairing_info(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    async fn connect(&self, _options: HashMap<String, OwnedValue>) -> Result<(), MockError> {
        self.counts.connect.fetch_add(1, Ordering::SeqCst);

        if !self.paired {
            return Err(MockError::NotPaired(format!("{} is not paired", self.id)));
        }
        if self.status == "offline" {
            return Err(MockError::NotReachable(format!("{} is offline", self.id)));
        }

        Ok(())
    }

    async fn disconnect(&self) {
        self.counts.disconnect.fetch_add(1, Ordering::SeqCst);
    }

    async fn scan(&self, _options: HashMap<String, OwnedValue>) -> OwnedObjectPath {
        self.counts.scan.fetch_add(1, Ordering::SeqCst);
        OwnedObjectPath::try_from(JOB_PATH).unwrap()
    }
}

struct ScannerWithoutScan {
    id: &'static str,
    name: &'static str,
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl ScannerWithoutScan {
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
        "mock://scanner".to_owned()
    }

    #[zbus(property)]
    fn capabilities(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    #[zbus(property)]
    fn supported_profiles(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        true
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
        "done".to_owned()
    }

    #[zbus(property)]
    fn pairing_error(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn pairing_info(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }
}

struct Job {
    scanner: &'static str,
    state: &'static str,
    result: Mutex<HashMap<String, OwnedValue>>,
}

#[zbus::interface(name = "org.scanbus.Job1")]
impl Job {
    #[zbus(property)]
    fn scanner(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(format!("/org/scanbus/scanner/{}", self.scanner)).unwrap()
    }

    #[zbus(property)]
    fn button(&self) -> i32 {
        -1
    }

    #[zbus(property)]
    fn profile(&self) -> String {
        "image".to_owned()
    }

    #[zbus(property)]
    fn state(&self) -> String {
        self.state.to_owned()
    }

    #[zbus(property)]
    fn page_count(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn result(&self) -> HashMap<String, OwnedValue> {
        self.result.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn error(&self) -> String {
        String::new()
    }
}

async fn serve_with_scan(address: &str, scanner: ScannerWithScan, job: Job) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager)
        .expect("/org/scanbus is a valid path")
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is a valid path")
        .serve_at(format!("/org/scanbus/scanner/{}", scanner.id), scanner)
        .expect("cannot export the scanner")
        .serve_at(JOB_PATH, job)
        .expect("cannot export the job")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus")
}

async fn serve_without_scan(address: &str, scanner: ScannerWithoutScan) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager)
        .expect("/org/scanbus is a valid path")
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is a valid path")
        .serve_at(format!("/org/scanbus/scanner/{}", scanner.id), scanner)
        .expect("cannot export the scanner")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus")
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_on_an_unpaired_scanner_exits_seven_and_mentions_pair() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("connect_on_an_unpaired_scanner_exits_seven_and_mentions_pair");
    };

    let counts = Counts::new();
    let _daemon = serve_with_scan(
        bus.address(),
        ScannerWithScan {
            id: UNPAIRED,
            name: "HP OfficeJet",
            paired: false,
            status: "online",
            counts: counts.clone(),
        },
        done_job(),
    )
    .await;

    let run = bus.scanbus(&["connect", UNPAIRED, "--id"]);
    run.assert_code(7);
    assert!(run.stderr.contains("run `scanbus pair"), "{}", run.stderr);
    assert_eq!(counts.connect.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_on_an_offline_scanner_exits_five_and_mentions_offline() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("connect_on_an_offline_scanner_exits_five_and_mentions_offline");
    };

    let counts = Counts::new();
    let _daemon = serve_with_scan(
        bus.address(),
        ScannerWithScan {
            id: OFFLINE,
            name: "Brother Offline",
            paired: true,
            status: "offline",
            counts: counts.clone(),
        },
        done_job(),
    )
    .await;

    let run = bus.scanbus(&["connect", OFFLINE, "--id"]);
    run.assert_code(5);
    assert!(run.stderr.contains("offline"), "{}", run.stderr);
    assert!(run.stderr.contains("paired"), "{}", run.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_rejects_an_unsupported_profile_before_calling_connect() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("connect_rejects_an_unsupported_profile_before_calling_connect");
    };

    let counts = Counts::new();
    let _daemon = serve_with_scan(
        bus.address(),
        ScannerWithScan {
            id: ONLINE,
            name: "Brother Online",
            paired: true,
            status: "online",
            counts: counts.clone(),
        },
        done_job(),
    )
    .await;

    let run = bus.scanbus(&["connect", ONLINE, "--id", "--profile", "ocr"]);
    run.assert_code(10);
    assert!(run.stderr.contains("image, document"), "{}", run.stderr);
    assert_eq!(counts.connect.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_is_quiet_and_idempotent_from_the_users_point_of_view() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("disconnect_is_quiet_and_idempotent_from_the_users_point_of_view");
    };

    let counts = Counts::new();
    let _daemon = serve_with_scan(
        bus.address(),
        ScannerWithScan {
            id: ONLINE,
            name: "Brother Online",
            paired: true,
            status: "online",
            counts: counts.clone(),
        },
        done_job(),
    )
    .await;

    let run = bus.scanbus(&["disconnect", ONLINE, "--id"]);
    run.assert_code(0);
    assert!(run.stdout.is_empty(), "{}", run.stdout);
    assert_eq!(counts.disconnect.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_prints_the_terminal_result_paths() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("scan_prints_the_terminal_result_paths");
    };

    let counts = Counts::new();
    let _daemon = serve_with_scan(
        bus.address(),
        ScannerWithScan {
            id: ONLINE,
            name: "Brother Online",
            paired: true,
            status: "online",
            counts: counts.clone(),
        },
        done_job(),
    )
    .await;

    let run = bus.scanbus(&["scan", ONLINE, "--id"]);
    run.assert_code(0);
    assert!(run.stdout.contains("/tmp/page-1.jpg"), "{}", run.stdout);
    assert!(run.stdout.contains("/tmp/page-2.jpg"), "{}", run.stdout);
    assert_eq!(counts.scan.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_no_wait_prints_the_short_job_id() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("scan_no_wait_prints_the_short_job_id");
    };

    let counts = Counts::new();
    let _daemon = serve_with_scan(
        bus.address(),
        ScannerWithScan {
            id: ONLINE,
            name: "Brother Online",
            paired: true,
            status: "online",
            counts: counts.clone(),
        },
        done_job(),
    )
    .await;

    let run = bus.scanbus(&["scan", ONLINE, "--id", "--no-wait"]);
    run.assert_code(0);
    assert_eq!(run.stdout.trim(), JOB_ID.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_reports_a_missing_method_cleanly() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("scan_reports_a_missing_method_cleanly");
    };

    let _daemon = serve_without_scan(
        bus.address(),
        ScannerWithoutScan {
            id: ONLINE,
            name: "Brother Online",
        },
    )
    .await;

    let run = bus.scanbus(&["scan", ONLINE, "--id"]);
    run.assert_code(1);
    assert!(
        run.stderr.contains("does not support host-driven scanning"),
        "{}",
        run.stderr
    );
}

fn done_job() -> Job {
    let paths = ZValue::from(vec!["/tmp/page-1.jpg", "/tmp/page-2.jpg"]);
    let result = HashMap::from([("paths".to_owned(), OwnedValue::try_from(paths).unwrap())]);

    Job {
        scanner: ONLINE,
        state: "done",
        result: Mutex::new(result),
    }
}
