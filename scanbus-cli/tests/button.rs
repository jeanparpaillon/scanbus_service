mod common;

use std::collections::{BTreeMap, HashMap};
use std::process::Command;
use std::sync::{Arc, Mutex};

use common::{PrivateBus, Run, skipped};
use scanbus_client::convert;
use scanbus_core::Value;
use zbus::zvariant::OwnedValue;

const BROTHER_23: &str = "brother_net_192_2E168_2E1_2E23";

struct Manager;

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    fn start_discovery(&self, _filters: HashMap<String, OwnedValue>) {}

    fn stop_discovery(&self) {}

    fn get_profile_types(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
    }
}

struct Scanner {
    id: String,
    name: String,
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl Scanner {
    #[zbus(property)]
    fn id(&self) -> String {
        self.id.clone()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.name.clone()
    }
}

#[derive(Debug, Clone)]
struct ButtonData {
    label: String,
    profile: String,
    profile_options: HashMap<String, OwnedValue>,
    set_label_calls: usize,
    set_profile_calls: usize,
    set_profile_options_calls: usize,
    fail_profile: bool,
}

impl ButtonData {
    fn options(&self) -> BTreeMap<String, Value> {
        convert::from_dict(&self.profile_options).unwrap()
    }
}

struct Button {
    index: u32,
    device_label: String,
    label_configurable: bool,
    state: Arc<Mutex<ButtonData>>,
}

#[zbus::interface(name = "org.scanbus.Button1")]
impl Button {
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.index
    }

    #[zbus(property)]
    fn device_label(&self) -> String {
        self.device_label.clone()
    }

    #[zbus(property)]
    fn label_configurable(&self) -> bool {
        self.label_configurable
    }

    #[zbus(property)]
    fn label(&self) -> String {
        self.state.lock().unwrap().label.clone()
    }

    #[zbus(property)]
    fn set_label(&self, value: &str) -> zbus::fdo::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.set_label_calls += 1;

        if !self.label_configurable {
            return Err(zbus::fdo::Error::PropertyReadOnly(
                "fixed device label".to_owned(),
            ));
        }

        state.label = value.to_owned();
        Ok(())
    }

    #[zbus(property)]
    fn profile(&self) -> String {
        self.state.lock().unwrap().profile.clone()
    }

    #[zbus(property)]
    fn set_profile(&self, value: &str) -> zbus::fdo::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.set_profile_calls += 1;

        if state.fail_profile {
            return Err(zbus::fdo::Error::Failed(
                "backend rewrite failed".to_owned(),
            ));
        }

        state.profile = value.to_owned();
        Ok(())
    }

    #[zbus(property)]
    fn profile_options(&self) -> HashMap<String, OwnedValue> {
        self.state.lock().unwrap().profile_options.clone()
    }

    #[zbus(property)]
    fn set_profile_options(&self, value: HashMap<String, OwnedValue>) {
        let mut state = self.state.lock().unwrap();
        state.set_profile_options_calls += 1;
        state.profile_options = value;
    }
}

#[derive(Clone)]
struct FixtureHandle {
    buttons: BTreeMap<u32, Arc<Mutex<ButtonData>>>,
}

impl FixtureHandle {
    fn button(&self, index: u32) -> ButtonData {
        self.buttons.get(&index).unwrap().lock().unwrap().clone()
    }

    fn fail_profile(&self, index: u32, fail: bool) {
        self.buttons
            .get(&index)
            .unwrap()
            .lock()
            .unwrap()
            .fail_profile = fail;
    }
}

async fn serve(address: &str) -> (zbus::Connection, FixtureHandle) {
    let connection = zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager)
        .expect("/org/scanbus is a valid path")
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is a valid path")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus");

    let server = connection.object_server();
    server
        .at(
            format!("/org/scanbus/scanner/{BROTHER_23}"),
            Scanner {
                id: BROTHER_23.to_owned(),
                name: "MFC-L2710DW".to_owned(),
            },
        )
        .await
        .expect("cannot export the scanner");

    let buttons = BTreeMap::from([
        (
            0,
            Arc::new(Mutex::new(ButtonData {
                label: "Scan to File".to_owned(),
                profile: "document".to_owned(),
                profile_options: convert::to_dict(&BTreeMap::from([(
                    "dir".to_owned(),
                    Value::Str("~/Documents/Scans".to_owned()),
                )])),
                set_label_calls: 0,
                set_profile_calls: 0,
                set_profile_options_calls: 0,
                fail_profile: false,
            })),
        ),
        (
            1,
            Arc::new(Mutex::new(ButtonData {
                label: "Scan to Image".to_owned(),
                profile: "image".to_owned(),
                profile_options: HashMap::new(),
                set_label_calls: 0,
                set_profile_calls: 0,
                set_profile_options_calls: 0,
                fail_profile: false,
            })),
        ),
        (
            2,
            Arc::new(Mutex::new(ButtonData {
                label: "Scan to OCR".to_owned(),
                profile: String::new(),
                profile_options: HashMap::new(),
                set_label_calls: 0,
                set_profile_calls: 0,
                set_profile_options_calls: 0,
                fail_profile: false,
            })),
        ),
        (
            3,
            Arc::new(Mutex::new(ButtonData {
                label: "Scan to E-mail".to_owned(),
                profile: String::new(),
                profile_options: HashMap::new(),
                set_label_calls: 0,
                set_profile_calls: 0,
                set_profile_options_calls: 0,
                fail_profile: false,
            })),
        ),
    ]);

    for (index, label) in [
        (0, "Scan to File"),
        (1, "Scan to Image"),
        (2, "Scan to OCR"),
        (3, "Scan to E-mail"),
    ] {
        server
            .at(
                format!("/org/scanbus/scanner/{BROTHER_23}/button/{index}"),
                Button {
                    index,
                    device_label: label.to_owned(),
                    label_configurable: false,
                    state: Arc::clone(buttons.get(&index).unwrap()),
                },
            )
            .await
            .expect("cannot export the button");
    }

    (connection, FixtureHandle { buttons })
}

async fn fixture() -> Option<(PrivateBus, zbus::Connection, FixtureHandle)> {
    let bus = PrivateBus::start()?;
    let (daemon, handle) = serve(bus.address()).await;
    Some((bus, daemon, handle))
}

fn scanbus_with_home(bus: &PrivateBus, args: &[&str], home: &str) -> Run {
    let mut all = vec!["--bus", bus.address()];
    all.extend_from_slice(args);

    let output = Command::new(env!("CARGO_BIN_EXE_scanbus"))
        .args(all)
        .env("HOME", home)
        .output()
        .expect("cannot run the scanbus binary");

    Run {
        code: output
            .status
            .code()
            .expect("the CLI was killed by a signal"),
        stdout: String::from_utf8(output.stdout).expect("stdout is not UTF-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is not UTF-8"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn button_list_shows_the_read_only_column_and_current_assignments() {
    let Some((bus, _daemon, _handle)) = fixture().await else {
        return skipped("button_list_shows_the_read_only_column_and_current_assignments");
    };

    let run = bus.scanbus(&["button", "list", "l2710"]);
    run.assert_code(0);
    assert!(run.stdout.contains("CONFIGURABLE"), "{}", run.stdout);
    assert!(run.stdout.contains("LABEL"), "{}", run.stdout);
    assert!(run.stdout.contains("Scan to File"), "{}", run.stdout);
    assert!(run.stdout.contains("no"), "{}", run.stdout);
    assert!(
        run.stdout.contains("dir=~/Documents/Scans"),
        "{}",
        run.stdout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn button_set_refuses_a_fixed_label_without_making_the_set_call() {
    let Some((bus, _daemon, handle)) = fixture().await else {
        return skipped("button_set_refuses_a_fixed_label_without_making_the_set_call");
    };

    let run = bus.scanbus(&["button", "set", "l2710", "2", "--label", "Contracts"]);
    run.assert_code(1);
    assert!(
        run.stderr.contains("PropertyReadOnly") && run.stderr.contains("--profile"),
        "{}",
        run.stderr
    );
    assert_eq!(handle.button(2).set_label_calls, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn button_set_parses_typed_options_expands_tilde_and_notes_divergence() {
    let Some((bus, _daemon, handle)) = fixture().await else {
        return skipped("button_set_parses_typed_options_expands_tilde_and_notes_divergence");
    };

    let run = scanbus_with_home(
        &bus,
        &[
            "button",
            "set",
            "l2710",
            "2",
            "--profile",
            "document",
            "--option",
            "pages=2",
            "--option",
            "dir=~/Scans",
        ],
        "/tmp/scanbus-home",
    );
    run.assert_code(0);
    assert!(
        run.stdout.contains("profile       document"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("note"), "{}", run.stdout);
    assert!(run.stdout.contains("Scan to OCR"), "{}", run.stdout);

    let state = handle.button(2);
    assert_eq!(state.profile, "document");
    assert_eq!(state.options()["pages"], Value::U64(2));
    assert_eq!(
        state.options()["dir"],
        Value::Str("/tmp/scanbus-home/Scans".to_owned())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn button_clear_unassigns_the_profile_and_options() {
    let Some((bus, _daemon, handle)) = fixture().await else {
        return skipped("button_clear_unassigns_the_profile_and_options");
    };

    let run = bus.scanbus(&["button", "clear", "l2710", "0"]);
    run.assert_code(0);

    let state = handle.button(0);
    assert!(state.profile.is_empty());
    assert!(state.options().is_empty());
    assert_eq!(state.set_profile_calls, 1);
    assert_eq!(state.set_profile_options_calls, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn button_set_reads_back_the_old_value_when_the_write_fails() {
    let Some((bus, _daemon, handle)) = fixture().await else {
        return skipped("button_set_reads_back_the_old_value_when_the_write_fails");
    };

    handle.fail_profile(1, true);

    let run = bus.scanbus(&["button", "set", "l2710", "1", "--profile", "document"]);
    run.assert_code(1);
    assert!(
        run.stderr.contains("backend rewrite failed"),
        "{}",
        run.stderr
    );
    assert!(run.stdout.contains("profile       image"), "{}", run.stdout);

    let state = handle.button(1);
    assert_eq!(state.profile, "image");
}
