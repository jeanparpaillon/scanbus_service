//! `Profile1.OptionsSchema`, on a real bus: the acceptance criteria of 10.13.
//!
//! The property exists so a client can build an editor without hard-coding this daemon's
//! accepted values ([`scanbus-dbus-api.md`] §6), and the only way to check that promise is
//! from the outside: read the schema, then write every value it lists and see it taken,
//! and write one it does not and see it refused. Asserting about the option table in a
//! unit test would prove the table is self-consistent, which is not the claim.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::HashMap;

use scanbus_core::ProfileKind;
use scanbus_daemon::Backends;
use scanbus_daemon::dbus::path;
use zbus::zvariant::{OwnedValue, Value as ZValue};

mod common;

use common::{Daemon, PrivateBus, skipped};

/// `org.scanbus.Profile1` as §6 defines it, from the client side.
///
/// Written out here rather than taken from `scanbus-client` for the reason the other
/// suites give: property caching is off, so every assertion is about what the daemon
/// serves now.
#[zbus::proxy(interface = "org.scanbus.Profile1", default_service = "org.scanbus")]
trait Profile {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn options(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn set_options(&self, value: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(property)]
    fn options_schema(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

async fn profile(connection: &zbus::Connection, kind: ProfileKind) -> ProfileProxy<'static> {
    ProfileProxy::builder(connection)
        .path(path::profile(kind))
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
}

/// One entry of the schema, as the `a{sv}` §6 says it is.
fn entry(schema: &HashMap<String, OwnedValue>, key: &str) -> HashMap<String, OwnedValue> {
    let value = schema
        .get(key)
        .unwrap_or_else(|| panic!("the schema has no {key}: {:?}", schema.keys()));
    HashMap::<String, OwnedValue>::try_from(value.clone())
        .unwrap_or_else(|error| panic!("{key} is not an a{{sv}}: {error}"))
}

fn text(entry: &HashMap<String, OwnedValue>, field: &str) -> String {
    String::try_from(entry[field].clone())
        .unwrap_or_else(|error| panic!("{field} is not a string: {error}"))
}

fn string(value: &str) -> OwnedValue {
    OwnedValue::try_from(ZValue::from(value)).unwrap()
}

/// Acceptance: reading the property answers with `format`, `quality` and `output_folder`,
/// each carrying what a client needs to render a row for it.
#[tokio::test]
async fn the_image_schema_carries_its_values_bounds_and_resolved_folder() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("the_image_schema_carries_its_values_bounds_and_resolved_folder");
    };
    let daemon = Daemon::start(&bus, Backends::default()).await;
    let client = bus.connect().await;

    let schema = profile(&client, ProfileKind::Image)
        .await
        .options_schema()
        .await
        .unwrap();

    let format = entry(&schema, "format");
    assert_eq!(text(&format, "type"), "string");
    assert_eq!(text(&format, "default"), "jpeg");
    assert_eq!(
        Vec::<String>::try_from(format["values"].clone()).unwrap(),
        ["jpeg", "jpg", "png"],
        "the three spellings §6 documents, aliases included"
    );

    let quality = entry(&schema, "quality");
    assert_eq!(text(&quality, "type"), "integer");
    assert_eq!(u64::try_from(quality["min"].clone()).unwrap(), 1);
    assert_eq!(u64::try_from(quality["max"].clone()).unwrap(), 100);

    let folder = entry(&schema, "output_folder");
    assert_eq!(text(&folder, "type"), "path");
    assert!(
        !text(&folder, "default").is_empty(),
        "an unset output_folder still has a directory behind it"
    );
    assert!(
        text(&folder, "default").ends_with("scanbus/image"),
        "the resolved default is under the profile's own directory: {}",
        text(&folder, "default")
    );

    // Every entry is renderable: §6 makes `type`, `default` and `description` mandatory,
    // and a client that falls back to a generic row needs all three.
    for key in schema.keys() {
        let entry = entry(&schema, key);
        for field in ["type", "default", "description"] {
            assert!(entry.contains_key(field), "{key} has no {field}");
        }
        assert!(!text(&entry, "description").is_empty(), "{key}");
    }

    daemon.shutdown().await;
}

/// Acceptance: `document` publishes its own key set, not `image`'s.
#[tokio::test]
async fn the_document_schema_is_the_documents_own() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("the_document_schema_is_the_documents_own");
    };
    let daemon = Daemon::start(&bus, Backends::default()).await;
    let client = bus.connect().await;

    let schema = profile(&client, ProfileKind::Document)
        .await
        .options_schema()
        .await
        .unwrap();

    let mut keys: Vec<&String> = schema.keys().collect();
    keys.sort();
    assert_eq!(keys, ["format", "multi_page", "output_folder"]);
    assert!(
        !schema.contains_key("quality"),
        "quality is an image option; publishing it here would invite a write that fails"
    );

    let format = entry(&schema, "format");
    assert_eq!(
        Vec::<String>::try_from(format["values"].clone()).unwrap(),
        ["pdf"]
    );

    let multi_page = entry(&schema, "multi_page");
    assert_eq!(text(&multi_page, "type"), "boolean");
    assert!(
        bool::try_from(multi_page["default"].clone()).unwrap(),
        "one PDF for the batch is what a scan does when nothing says otherwise"
    );

    daemon.shutdown().await;
}

/// Acceptance: the schema is honest in both directions — everything it lists is written
/// without error, and a value it does not list is `InvalidArgs` and changes nothing.
#[tokio::test]
async fn what_the_schema_lists_is_accepted_and_what_it_omits_is_refused() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("what_the_schema_lists_is_accepted_and_what_it_omits_is_refused");
    };
    let daemon = Daemon::start(&bus, Backends::default()).await;
    let client = bus.connect().await;
    let image = profile(&client, ProfileKind::Image).await;

    let schema = image.options_schema().await.unwrap();
    let formats = Vec::<String>::try_from(entry(&schema, "format")["values"].clone()).unwrap();

    for format in &formats {
        image
            .set_options(HashMap::from([("format".to_owned(), string(format))]))
            .await
            .unwrap_or_else(|error| {
                panic!("the schema lists {format:?} but the daemon refuses it: {error}")
            });
    }

    let before = image.options().await.unwrap();

    // A format outside the published set.
    let error = image
        .set_options(HashMap::from([("format".to_owned(), string("tiff"))]))
        .await
        .expect_err("tiff is not in values");
    assert!(
        error.to_string().contains("jpeg/jpg/png"),
        "the refusal names what is accepted: {error}"
    );

    // A key the schema does not declare at all.
    let error = image
        .set_options(HashMap::from([(
            "sharpening".to_owned(),
            string("aggressive"),
        )]))
        .await
        .expect_err("an undeclared key is not an option");
    assert!(error.to_string().contains("sharpening"), "{error}");

    // A value outside the published bounds.
    let error = image
        .set_options(HashMap::from([(
            "quality".to_owned(),
            OwnedValue::try_from(ZValue::from(200u32)).unwrap(),
        )]))
        .await
        .expect_err("200 is past max");
    assert!(error.to_string().contains("1..=100"), "{error}");

    assert_eq!(
        image.options().await.unwrap(),
        before,
        "a refused write leaves the stored options exactly as they were"
    );

    daemon.shutdown().await;
}
