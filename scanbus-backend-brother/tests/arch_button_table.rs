//! [`Function`] against the two arch tables it is a copy of.
//!
//! The unit tests in `src/skey/` assert the four entries against a table written out a
//! second time in the test itself, which proves the four `match` arms agree with each
//! other and nothing more: someone renumbering `APPNUM` would renumber both copies in one
//! edit and the suite would stay green. These read the markdown instead —
//! `scanbus-dbus-api.md` §5's worked example for index ↔ `DeviceLabel`, and
//! `brother-skeyless-backend.md` §3 for the `FUNC` and `APPNUM` that go on the wire — so
//! the contract is the input and the code is what is being checked.
//!
//! `include_str!` rather than a runtime read: the path is resolved at compile time, so a
//! doc that moves breaks the build here instead of silently skipping the assertions.
//!
//! What this cannot check is the firmware. That the panel really labels APPNUM 2
//! "Scan to E-mail" is the acceptance criterion on a real MFC-J5335DW; this only makes
//! sure the daemon and the documents say the same thing about it.

use scanbus_backend_brother::skey::function::Function;
use scanbus_core::{ProfileKind, implied_profile};

const API: &str = include_str!("../../docs/scanbus-dbus-api.md");
const BROTHER: &str = include_str!("../../docs/brother-skeyless-backend.md");

/// The headings the two tables live under. Named rather than repeated, so a section that
/// is renamed is one edit and not four silent `unwrap`s on the wrong half of a document.
const API_EXAMPLE: &str = "### Concrete example (Brother MFC, 4 fixed keys)";
const WIRE_TABLE: &str = "**Buttons are registrations.**";

/// The rows of the first markdown table after `heading`, cells trimmed and unmarked.
///
/// Deliberately blunt: find the heading, take the first run of `|` lines under it, drop
/// the `---` separator and the header row, and strip the backticks and quotes the docs
/// use for emphasis. Anything more forgiving would start papering over a table that had
/// been restructured, which is the case where this test has to fail rather than adapt.
fn table_after(doc: &str, heading: &str) -> Vec<Vec<String>> {
    let body = doc
        .split_once(heading)
        .unwrap_or_else(|| panic!("no {heading:?} in the document; the section was renamed"))
        .1;

    body.lines()
        .skip_while(|line| !line.trim_start().starts_with('|'))
        .take_while(|line| line.trim_start().starts_with('|'))
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().trim_matches('`').trim_matches('"').to_owned())
                .collect::<Vec<_>>()
        })
        // The header row and the `| --- |` under it carry no data.
        .filter(|cells| !cells.iter().all(|cell| cell.chars().all(|c| c == '-')))
        .skip(1)
        .collect()
}

/// API §5: index, `DeviceLabel`, `LabelConfigurable`, and the profile it suggests.
#[test]
fn the_api_worked_example_is_what_the_backend_publishes() {
    let rows = table_after(API, API_EXAMPLE);
    assert_eq!(
        rows.len(),
        Function::ALL.len(),
        "§5's example lists {} keys, the backend has {}",
        rows.len(),
        Function::ALL.len()
    );

    for row in &rows {
        let [index, label, configurable, profile] = row.as_slice() else {
            panic!("§5's example no longer has four columns: {row:?}");
        };
        let index: u32 = index.parse().expect("§5 numbers its keys");
        let function = Function::from_button_index(index)
            .unwrap_or_else(|| panic!("§5 documents key {index}, the backend has no such entry"));

        assert_eq!(
            function.device_label(),
            label,
            "key {index} is {:?} in §5 and {:?} here",
            label,
            function.device_label()
        );
        // `LabelConfigurable` is `false` for every Brother key, so nothing in this crate
        // reads a label back from a client. A row saying otherwise would mean the panel
        // grew something to rename, which is a design change and not a test failure to
        // paper over.
        assert_eq!(configurable, "false", "key {index} in §5");

        // The label is not decoration: `implied_profile` is what makes the CLI and GUI
        // warn when the assigned profile diverges from the engraving, and it reads this
        // string. That it lands on the profile §5 calls assignable is the whole reason
        // the exact wording — "Scan to E-mail", not "E-Mail" — has to match.
        assert_eq!(
            implied_profile(function.device_label()).map(ProfileKind::as_str),
            Some(profile.as_str()),
            "key {index}: {:?} does not read as the {profile:?} §5 assigns it",
            function.device_label()
        );
    }
}

/// `brother-skeyless-backend.md` §3: the same indices, plus what goes on the wire.
#[test]
fn the_wire_table_is_the_one_the_brother_design_documents() {
    let rows = table_after(BROTHER, WIRE_TABLE);
    assert_eq!(rows.len(), Function::ALL.len());

    for row in &rows {
        let [index, func, appnum, label] = row.as_slice() else {
            panic!("§3's table no longer has four columns: {row:?}");
        };
        let index: u32 = index.parse().expect("§3 numbers its keys");
        let appnum: u8 = appnum.parse().expect("§3's APPNUM is a number");

        let function = Function::from_button_index(index)
            .unwrap_or_else(|| panic!("§3 documents key {index}, the backend has no such entry"));
        assert_eq!(function.as_str(), func, "key {index}: FUNC");
        assert_eq!(function.appnum(), appnum, "key {index}: APPNUM");
        assert_eq!(function.device_label(), label, "key {index}: DeviceLabel");

        // The reverse lookups are what registration (5.8) and event decoding (5.9)
        // actually call, and a table that only reads correctly in one direction would
        // still turn a press on key `index` into some other button.
        assert_eq!(Function::from_token(func), Some(function));
        assert_eq!(Function::from_appnum(appnum), Some(function));
    }
}

/// The two documents describe the same four keys, and so does the enum.
#[test]
fn both_documents_and_the_enum_agree_key_for_key() {
    let api: Vec<(String, String)> = table_after(API, API_EXAMPLE)
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    let brother: Vec<(String, String)> = table_after(BROTHER, WIRE_TABLE)
        .into_iter()
        .map(|row| (row[0].clone(), row[3].clone()))
        .collect();
    assert_eq!(api, brother, "§5 and §3 disagree about index or label");

    let ours: Vec<(String, String)> = Function::ALL
        .iter()
        .map(|f| (f.button_index().to_string(), f.device_label().to_owned()))
        .collect();
    assert_eq!(ours, api);

    // `Function::ALL` is documented as being in `button_index` order, and both the
    // published `buttons.count` and the label vector index into it positionally.
    for (position, function) in Function::ALL.iter().enumerate() {
        assert_eq!(u32::try_from(position).unwrap(), function.button_index());
    }
}

/// An index outside the table has no function, whatever the shape of the caller.
#[test]
fn an_index_the_table_does_not_hold_resolves_to_nothing() {
    for index in [Function::ALL.len() as u32, 9, u32::MAX] {
        assert_eq!(Function::from_button_index(index), None, "index {index}");
    }
    // APPNUM 4 sits inside the range the four entries span and is still not one of them.
    assert_eq!(Function::from_appnum(4), None);
    assert_eq!(Function::from_appnum(0), None);
    assert_eq!(Function::from_token("SCAN"), None);
    assert_eq!(
        Function::from_token("file"),
        None,
        "the token is upper-case"
    );
}
