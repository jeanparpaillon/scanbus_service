mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::{PrivateBus, skipped};
use scanbus_client::convert;
use scanbus_core::Value;
use zbus::zvariant::OwnedValue;

const SCANNER: &str = "brother_net_192_2E168_2E1_2E23";

struct Manager;

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    fn start_discovery(&self, _filters: HashMap<String, OwnedValue>) {}

    fn stop_discovery(&self) {}

    fn get_profile_types(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
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
}

#[derive(Debug, Clone)]
struct ButtonData {
    profile: String,
    profile_options: HashMap<String, OwnedValue>,
}

struct Button {
    index: u32,
    device_label: String,
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
    fn profile(&self) -> String {
        self.state.lock().unwrap().profile.clone()
    }

    #[zbus(property)]
    fn profile_options(&self) -> HashMap<String, OwnedValue> {
        self.state.lock().unwrap().profile_options.clone()
    }
}

#[derive(Debug, Clone)]
struct ProfileData {
    options: HashMap<String, OwnedValue>,
    options_schema: HashMap<String, OwnedValue>,
}

impl ProfileData {
    fn options(&self) -> BTreeMap<String, Value> {
        convert::from_dict(&self.options).unwrap()
    }
}

struct Profile {
    name: &'static str,
    state: Arc<Mutex<ProfileData>>,
}

#[zbus::interface(name = "org.scanbus.Profile1")]
impl Profile {
    #[zbus(property)]
    fn name(&self) -> String {
        self.name.to_owned()
    }

    #[zbus(property)]
    fn options(&self) -> HashMap<String, OwnedValue> {
        self.state.lock().unwrap().options.clone()
    }

    #[zbus(property)]
    fn options_schema(&self) -> HashMap<String, OwnedValue> {
        self.state.lock().unwrap().options_schema.clone()
    }

    #[zbus(property)]
    fn set_options(&self, value: HashMap<String, OwnedValue>) -> zbus::fdo::Result<()> {
        let parsed = convert::from_dict(&value)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        validate_options(self.name, &parsed)?;
        self.state.lock().unwrap().options = convert::to_dict(&parsed);
        Ok(())
    }
}

#[derive(Clone)]
struct FixtureHandle {
    profiles: BTreeMap<&'static str, Arc<Mutex<ProfileData>>>,
}

impl FixtureHandle {
    fn profile(&self, name: &'static str) -> ProfileData {
        self.profiles.get(name).unwrap().lock().unwrap().clone()
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
        .at(format!("/org/scanbus/scanner/{SCANNER}"), Scanner)
        .await
        .expect("cannot export the scanner");

    let button = Arc::new(Mutex::new(ButtonData {
        profile: "document".to_owned(),
        profile_options: convert::to_dict(&BTreeMap::from([(
            "output_folder".to_owned(),
            Value::Str("/tmp/contracts".to_owned()),
        )])),
    }));
    server
        .at(
            format!("/org/scanbus/scanner/{SCANNER}/button/2"),
            Button {
                index: 2,
                device_label: "Scan to OCR".to_owned(),
                state: Arc::clone(&button),
            },
        )
        .await
        .expect("cannot export the button");

    let profiles = BTreeMap::from([
        (
            "image",
            Arc::new(Mutex::new(ProfileData {
                options: convert::to_dict(&BTreeMap::from([
                    ("format".to_owned(), Value::Str("jpeg".to_owned())),
                    ("quality".to_owned(), Value::U64(90)),
                    (
                        "output_folder".to_owned(),
                        Value::Str("/tmp/images".to_owned()),
                    ),
                ])),
                options_schema: image_schema(),
            })),
        ),
        (
            "document",
            Arc::new(Mutex::new(ProfileData {
                options: convert::to_dict(&BTreeMap::from([
                    ("format".to_owned(), Value::Str("pdf".to_owned())),
                    ("multi_page".to_owned(), Value::Bool(true)),
                    (
                        "output_folder".to_owned(),
                        Value::Str("/tmp/documents".to_owned()),
                    ),
                ])),
                options_schema: document_schema(),
            })),
        ),
    ]);

    for (name, state) in &profiles {
        server
            .at(
                format!("/org/scanbus/profile/{name}"),
                Profile {
                    name,
                    state: Arc::clone(state),
                },
            )
            .await
            .expect("cannot export the profile");
    }

    (connection, FixtureHandle { profiles })
}

async fn fixture() -> Option<(PrivateBus, zbus::Connection, FixtureHandle)> {
    let bus = PrivateBus::start()?;
    let (daemon, handle) = serve(bus.address()).await;
    Some((bus, daemon, handle))
}

fn validate_options(name: &str, options: &BTreeMap<String, Value>) -> zbus::fdo::Result<()> {
    match name {
        "image" => {
            if let Some(value) = options.get("quality")
                && !matches!(value, Value::U64(_) | Value::I64(_))
            {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "image quality must be an integer".to_owned(),
                ));
            }
            if let Some(value) = options.get("format")
                && !matches!(value, Value::Str(_))
            {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "image format must be a string".to_owned(),
                ));
            }
        }
        "document" => {
            if let Some(value) = options.get("multi_page")
                && !matches!(value, Value::Bool(_))
            {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "document multi_page must be a boolean".to_owned(),
                ));
            }
        }
        _ => {}
    }

    if let Some(value) = options.get("output_folder")
        && !matches!(value, Value::Str(_))
    {
        return Err(zbus::fdo::Error::InvalidArgs(
            "output_folder must be a string".to_owned(),
        ));
    }

    Ok(())
}

fn image_schema() -> HashMap<String, OwnedValue> {
    convert::to_dict(&BTreeMap::from([
        (
            "format".to_owned(),
            Value::Dict(BTreeMap::from([
                ("type".to_owned(), Value::Str("string".to_owned())),
                ("default".to_owned(), Value::Str("jpeg".to_owned())),
                (
                    "values".to_owned(),
                    Value::Array(vec![
                        Value::Str("jpeg".to_owned()),
                        Value::Str("jpg".to_owned()),
                        Value::Str("png".to_owned()),
                    ]),
                ),
                (
                    "description".to_owned(),
                    Value::Str("Encoding of the written page files".to_owned()),
                ),
            ])),
        ),
        (
            "quality".to_owned(),
            Value::Dict(BTreeMap::from([
                ("type".to_owned(), Value::Str("integer".to_owned())),
                ("default".to_owned(), Value::U64(90)),
                ("min".to_owned(), Value::U64(1)),
                ("max".to_owned(), Value::U64(100)),
                (
                    "description".to_owned(),
                    Value::Str("JPEG quality; ignored when format is png".to_owned()),
                ),
            ])),
        ),
        (
            "output_folder".to_owned(),
            Value::Dict(BTreeMap::from([
                ("type".to_owned(), Value::Str("path".to_owned())),
                ("default".to_owned(), Value::Str("/tmp/images".to_owned())),
                (
                    "description".to_owned(),
                    Value::Str("Directory the pages are written to".to_owned()),
                ),
            ])),
        ),
    ]))
}

fn document_schema() -> HashMap<String, OwnedValue> {
    convert::to_dict(&BTreeMap::from([
        (
            "format".to_owned(),
            Value::Dict(BTreeMap::from([
                ("type".to_owned(), Value::Str("string".to_owned())),
                ("default".to_owned(), Value::Str("pdf".to_owned())),
                (
                    "values".to_owned(),
                    Value::Array(vec![
                        Value::Str("pdf".to_owned()),
                        Value::Str("jpeg".to_owned()),
                    ]),
                ),
                (
                    "description".to_owned(),
                    Value::Str("Encoding of the written document files".to_owned()),
                ),
            ])),
        ),
        (
            "multi_page".to_owned(),
            Value::Dict(BTreeMap::from([
                ("type".to_owned(), Value::Str("boolean".to_owned())),
                ("default".to_owned(), Value::Bool(true)),
                (
                    "description".to_owned(),
                    Value::Str("One file for the batch".to_owned()),
                ),
            ])),
        ),
        (
            "output_folder".to_owned(),
            Value::Dict(BTreeMap::from([
                ("type".to_owned(), Value::Str("path".to_owned())),
                (
                    "default".to_owned(),
                    Value::Str("/tmp/documents".to_owned()),
                ),
                (
                    "description".to_owned(),
                    Value::Str("Directory the pages are written to".to_owned()),
                ),
            ])),
        ),
    ]))
}

fn missing_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "scanbus-profile-missing-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_list_reports_only_the_exported_profiles() {
    let Some((bus, _daemon, _handle)) = fixture().await else {
        return skipped("profile_list_reports_only_the_exported_profiles");
    };

    let run = bus.scanbus(&["profile", "list"]);
    run.assert_code(0);
    assert!(run.stdout.contains("image"), "{}", run.stdout);
    assert!(run.stdout.contains("document"), "{}", run.stdout);
    assert!(!run.stdout.contains("email"), "{}", run.stdout);
    assert!(!run.stdout.contains("ocr"), "{}", run.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_show_lists_global_options_and_button_overrides() {
    let Some((bus, _daemon, _handle)) = fixture().await else {
        return skipped("profile_show_lists_global_options_and_button_overrides");
    };

    let run = bus.scanbus(&["profile", "show", "document"]);
    run.assert_code(0);
    assert!(run.stdout.contains("/tmp/documents"), "{}", run.stdout);
    assert!(run.stdout.contains(SCANNER), "{}", run.stdout);
    assert!(run.stdout.contains("Scan to OCR"), "{}", run.stdout);
    assert!(run.stdout.contains("/tmp/contracts"), "{}", run.stdout);
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_show_json_includes_the_image_option_schema() {
    let Some((bus, _daemon, _handle)) = fixture().await else {
        return skipped("profile_show_json_includes_the_image_option_schema");
    };

    let json = bus
        .scanbus(&["--json", "profile", "show", "image"])
        .assert_code(0)
        .json();

    assert_eq!(json["Options"]["format"], "jpeg");
    assert_eq!(json["OptionsSchema"]["format"]["type"], "string");
    assert_eq!(
        json["OptionsSchema"]["format"]["values"],
        serde_json::json!(["jpeg", "jpg", "png"])
    );
    assert_eq!(json["OptionsSchema"]["quality"]["min"], 1);
    assert_eq!(json["OptionsSchema"]["quality"]["max"], 100);
    assert_eq!(
        json["OptionsSchema"]["output_folder"]["default"],
        "/tmp/images"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_show_json_includes_the_document_option_schema() {
    let Some((bus, _daemon, _handle)) = fixture().await else {
        return skipped("profile_show_json_includes_the_document_option_schema");
    };

    let json = bus
        .scanbus(&["--json", "profile", "show", "document"])
        .assert_code(0)
        .json();

    assert_eq!(json["Options"]["format"], "pdf");
    assert_eq!(
        json["OptionsSchema"]["format"]["values"],
        serde_json::json!(["pdf", "jpeg"])
    );
    assert_eq!(json["OptionsSchema"]["multi_page"]["type"], "boolean");
    assert_eq!(json["OptionsSchema"]["multi_page"]["default"], true);
    assert!(
        json["OptionsSchema"]["quality"].is_null(),
        "{}",
        json["OptionsSchema"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_profile_exits_four_and_lists_the_known_ones() {
    let Some((bus, _daemon, _handle)) = fixture().await else {
        return skipped("an_unknown_profile_exits_four_and_lists_the_known_ones");
    };

    let run = bus.scanbus(&["profile", "show", "nosuch"]);
    run.assert_code(4);
    assert!(run.stderr.contains("no profile matches"), "{}", run.stderr);
    assert!(run.stderr.contains("image"), "{}", run.stderr);
    assert!(run.stderr.contains("document"), "{}", run.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_set_preserves_existing_options_and_writes_typed_values() {
    let Some((bus, _daemon, handle)) = fixture().await else {
        return skipped("profile_set_preserves_existing_options_and_writes_typed_values");
    };

    let run = bus.scanbus(&[
        "--json",
        "profile",
        "set",
        "image",
        "quality=85",
        "format=png",
    ]);
    run.assert_code(0);
    let json = run.json();
    assert_eq!(json["Options"]["quality"], 85);
    assert_eq!(json["Options"]["format"], "png");
    assert_eq!(json["Options"]["output_folder"], "/tmp/images");

    let stored = handle.profile("image").options();
    assert_eq!(stored["quality"], Value::U64(85));
    assert_eq!(stored["format"], Value::Str("png".to_owned()));
    assert_eq!(
        stored["output_folder"],
        Value::Str("/tmp/images".to_owned())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_set_warns_on_a_missing_output_directory_but_still_writes() {
    let Some((bus, _daemon, handle)) = fixture().await else {
        return skipped("profile_set_warns_on_a_missing_output_directory_but_still_writes");
    };

    let missing = missing_dir();
    let path = missing.display().to_string();

    let run = bus.scanbus(&[
        "--json",
        "profile",
        "set",
        "document",
        &format!("output_folder={path}"),
    ]);
    run.assert_code(0);
    assert!(run.stderr.contains("warning"), "{}", run.stderr);
    assert!(run.stderr.contains(&path), "{}", run.stderr);
    assert_eq!(run.json()["Options"]["output_folder"], path);
    assert_eq!(
        handle.profile("document").options()["output_folder"],
        Value::Str(path)
    );
}
