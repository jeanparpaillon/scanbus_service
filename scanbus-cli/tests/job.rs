//! `job list`, `job show` and `job watch` against a private bus ([8.8]).

mod common;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use common::{PrivateBus, skipped};
use tokio::time::sleep;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

const SCANNER: &str = "brother_net_192_2E168_2E1_2E23";
const JOB_PATH: &str = "/org/scanbus/scanner/brother_net_192_2E168_2E1_2E23/job/7";

struct Manager;

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    fn get_profile_types(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned(), "ocr".to_owned()]
    }
}

struct Scanner;

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl Scanner {
    #[zbus(property)]
    fn id(&self) -> String {
        SCANNER.to_owned()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        "MFC-L2710DW".to_owned()
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
        vec!["image".to_owned(), "document".to_owned(), "ocr".to_owned()]
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
}

struct Button;

#[zbus::interface(name = "org.scanbus.Button1")]
impl Button {
    #[zbus(property)]
    fn index(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn device_label(&self) -> String {
        "Scan to OCR".to_owned()
    }
}

struct Job {
    state: Mutex<String>,
    page_count: Mutex<u32>,
    result: Mutex<HashMap<String, OwnedValue>>,
    error: Mutex<String>,
}

impl Job {
    fn new() -> Self {
        Self {
            state: Mutex::new("receiving".to_owned()),
            page_count: Mutex::new(1),
            result: Mutex::new(HashMap::new()),
            error: Mutex::new(String::new()),
        }
    }
}

#[zbus::interface(name = "org.scanbus.Job1")]
impl Job {
    #[zbus(property)]
    fn scanner(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(format!("/org/scanbus/scanner/{SCANNER}")).unwrap()
    }

    #[zbus(property)]
    fn button(&self) -> i32 {
        2
    }

    #[zbus(property)]
    fn profile(&self) -> String {
        "document".to_owned()
    }

    #[zbus(property)]
    fn state(&self) -> String {
        self.state.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn page_count(&self) -> u32 {
        *self.page_count.lock().unwrap()
    }

    #[zbus(property)]
    fn result(&self) -> HashMap<String, OwnedValue> {
        self.result.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.error.lock().unwrap().clone()
    }
}

async fn serve(address: &str) -> zbus::Connection {
    let connection = zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager)
        .expect("/org/scanbus is valid")
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is valid")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus");

    let server = connection.object_server();
    server
        .at(format!("/org/scanbus/scanner/{SCANNER}"), Scanner)
        .await
        .unwrap();
    server
        .at(format!("/org/scanbus/scanner/{SCANNER}/button/2"), Button)
        .await
        .unwrap();
    server.at(JOB_PATH, Job::new()).await.unwrap();

    connection
}

async fn serve_for_watch(address: &str) -> zbus::Connection {
    let connection = zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager)
        .expect("/org/scanbus is valid")
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is valid")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus");

    let server = connection.object_server();
    server
        .at(format!("/org/scanbus/scanner/{SCANNER}"), Scanner)
        .await
        .unwrap();
    server
        .at(format!("/org/scanbus/scanner/{SCANNER}/button/2"), Button)
        .await
        .unwrap();

    let spawned = connection.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        spawned.object_server().at(JOB_PATH, Job::new()).await.unwrap();

        sleep(Duration::from_millis(50)).await;
        let iface: InterfaceRef<Job> = spawned.object_server().interface(JOB_PATH).await.unwrap();
        {
            let job = iface.get().await;
            *job.page_count.lock().unwrap() = 2;
            job.page_count_changed(iface.signal_emitter()).await.unwrap();
        }

        sleep(Duration::from_millis(50)).await;
        {
            let job = iface.get().await;
            *job.state.lock().unwrap() = "processing".to_owned();
            job.state_changed(iface.signal_emitter()).await.unwrap();
        }

        sleep(Duration::from_millis(50)).await;
        {
            let job = iface.get().await;
            *job.state.lock().unwrap() = "done".to_owned();
            job.result.lock().unwrap().insert(
                "path".to_owned(),
                OwnedValue::try_from(ZValue::from("/tmp/scan.pdf")).unwrap(),
            );
            job.state_changed(iface.signal_emitter()).await.unwrap();
            job.result_changed(iface.signal_emitter()).await.unwrap();
        }
    });

    connection
}

fn json_lines(output: &str) -> Vec<serde_json::Value> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn job_list_and_show_report_jobs_with_button_context() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("job_list_and_show_report_jobs_with_button_context");
    };
    let _daemon = serve(bus.address()).await;

    let jobs = bus.scanbus(&["--json", "job", "list"]).assert_code(0).json();
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    assert_eq!(jobs[0]["Scanner"], format!("/org/scanbus/scanner/{SCANNER}"));
    assert_eq!(jobs[0]["State"], "receiving");
    assert_eq!(jobs[0]["PageCount"], 1);

    let show = bus.scanbus(&["--json", "job", "show", "7"]).assert_code(0).json();
    assert_eq!(show["ButtonDeviceLabel"], "Scan to OCR");
    assert_eq!(show["ButtonLabelDivergesFromProfile"], true);
    assert_eq!(show["Profile"], "document");
    assert_eq!(show["Button"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn job_watch_until_done_streams_the_job_lifecycle() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("job_watch_until_done_streams_the_job_lifecycle");
    };
    let _daemon = serve_for_watch(bus.address()).await;

    let run = bus.scanbus(&["--json", "job", "watch", "--until-done"]);
    run.assert_code(0);
    let events = json_lines(&run.stdout);

    assert!(events.len() >= 4, "{events:?}");
    assert_eq!(events[0]["State"], "receiving");
    assert_eq!(events[0]["PageCount"], 1);
    assert!(events.iter().any(|event| event["PageCount"] == 2), "{events:?}");
    assert!(events.iter().any(|event| event["State"] == "processing"), "{events:?}");
    assert_eq!(events.last().unwrap()["State"], "done");
    assert_eq!(events.last().unwrap()["Result"]["path"], "/tmp/scan.pdf");
}
