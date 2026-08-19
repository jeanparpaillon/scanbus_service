//! `.blp` → `.ui` → `.gresource`, the three steps of scanbus-gnome-gui.md §2.1.
//!
//! Nothing this script produces is checked in: a committed generated `.ui` is one
//! someone edits the `.blp` without regenerating, and the first symptom is a widget
//! that silently keeps its old shape. Everything lands in `OUT_DIR` and is linked
//! into the binary by `gio::resources_register_include!`.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

/// The resource prefix the Rust side looks templates up under.
const RESOURCE_PREFIX: &str = "/org/scanbus/Gui/ui";

/// What `debian/control` and the CI jobs install. Named in the error so a missing
/// compiler is one `apt-get install` away rather than a raw spawn failure.
const COMPILER_PACKAGE: &str = "blueprint-compiler";

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let ui_src = PathBuf::from("data/ui");
    let ui_out = out_dir.join("ui");

    let blueprints = blueprints(&ui_src);
    assert!(
        !blueprints.is_empty(),
        "no .blp found in {}: the glob matched nothing, which would produce an empty \
         gresource and a template that will not instantiate",
        ui_src.display()
    );

    // A new or removed .blp changes the file list, which the per-file lines below
    // cannot notice on their own.
    println!("cargo:rerun-if-changed={}", ui_src.display());
    for blp in &blueprints {
        println!("cargo:rerun-if-changed={}", blp.display());
    }

    fs::create_dir_all(&ui_out).expect("create OUT_DIR/ui");
    batch_compile(&ui_src, &ui_out, &blueprints);

    let manifest = write_gresource_manifest(&out_dir, &blueprints);
    glib_build_tools::compile_resources(
        &[&ui_out],
        manifest.to_str().expect("OUT_DIR is UTF-8"),
        "scanbus.gresource",
    );
}

/// Every `data/ui/*.blp`, sorted so the manifest is stable across builds.
fn blueprints(ui_src: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(ui_src)
        .unwrap_or_else(|e| panic!("read {}: {e}", ui_src.display()))
        .map(|entry| entry.expect("read dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "blp"))
        .collect();
    found.sort();
    found
}

/// `blueprint-compiler batch-compile OUT_DIR/ui data/ui data/ui/*.blp`.
fn batch_compile(ui_src: &Path, ui_out: &Path, blueprints: &[PathBuf]) {
    let mut command = Command::new("blueprint-compiler");
    command.arg("batch-compile").arg(ui_out).arg(ui_src);
    command.args(blueprints);

    let output = match command.output() {
        Ok(output) => output,
        Err(e) if e.kind() == ErrorKind::NotFound => panic!(
            "`blueprint-compiler` not found on PATH. scanbus-gui compiles its widget \
             tree from data/ui/*.blp at build time; install the `{COMPILER_PACKAGE}` \
             package (Debian/Ubuntu: `sudo apt-get install {COMPILER_PACKAGE}`). \
             Crates other than scanbus-gui do not need it — it is not in \
             default-members."
        ),
        Err(e) => panic!("running blueprint-compiler: {e}"),
    };

    if !output.status.success() {
        // Verbatim, so a Blueprint syntax error keeps its file, line and caret in
        // `cargo build` output instead of being reformatted into a panic string.
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        panic!(
            "blueprint-compiler failed ({}); its output is above",
            output.status
        );
    }
}

/// The `.gresource.xml` glib-compile-resources needs, generated rather than committed
/// for the same reason the `.ui` is: it lists exactly what the `.blp` glob found.
fn write_gresource_manifest(out_dir: &Path, blueprints: &[PathBuf]) -> PathBuf {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gresources>\n");
    xml.push_str(&format!("  <gresource prefix=\"{RESOURCE_PREFIX}\">\n"));
    for blp in blueprints {
        let stem = blp
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| panic!("non-UTF-8 blueprint name: {}", blp.display()));
        xml.push_str(&format!(
            "    <file compressed=\"true\" preprocess=\"xml-stripblanks\">{stem}.ui</file>\n"
        ));
    }
    xml.push_str("  </gresource>\n</gresources>\n");

    let manifest = out_dir.join("scanbus.gresource.xml");
    fs::write(&manifest, xml).unwrap_or_else(|e| panic!("write {}: {e}", manifest.display()));
    manifest
}
