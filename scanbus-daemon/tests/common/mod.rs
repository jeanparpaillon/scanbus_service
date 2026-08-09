//! A `dbus-daemon` of our own, for tests that need a real bus.
//!
//! A submodule rather than a `tests/*.rs` of its own, so cargo does not compile it as
//! an integration test in its own right. The alternative — asserting about the object
//! tree without a bus — is what 2.1 exists to stop, since every failure it is about (a
//! name taken, a `GetManagedObjects` that errors, an object that outlives its unexport)
//! only happens on a bus.
//!
//! The bus is started with a configuration written here rather than the distribution's
//! `session.conf`, so that the suite depends on the `dbus-daemon` binary and nothing
//! else: no service directories, no per-distro policy, no chance of talking to the
//! developer's real session bus by accident.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Minimal session-bus policy: anything may be owned, sent and received.
///
/// All three rules are needed. Without `receive_sender`, `dbus-daemon` denies the
/// method *returns* and the `NameAcquired` signal the bus itself sends back, and every
/// call hangs until its timeout — which looks exactly like a daemon that is not
/// answering. `receive_sender` rather than the `eavesdrop="true"` the distribution's
/// `session.conf` still uses: nothing here listens to traffic addressed elsewhere.
const CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC
 "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=SOCKET_DIR</listen>
  <policy context="default">
    <allow own="*"/>
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
  </policy>
</busconfig>
"#;

/// A private session bus, killed and cleaned up when this is dropped.
pub struct PrivateBus {
    address: String,
    dir: PathBuf,
    // Kept alive for the lifetime of the bus; `kill_on_drop` does the rest.
    _child: Child,
}

impl PrivateBus {
    /// Starts a bus, or returns `None` when `dbus-daemon` is not installed.
    ///
    /// Skipping rather than failing is the deal with CI: the D-Bus daemon is not a
    /// build dependency of this crate, and a runner without one should not turn a
    /// green suite red. Anything else — a daemon that starts and then dies, an address
    /// that never arrives — is a real failure and panics.
    pub async fn start() -> Option<Self> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let dir = std::env::temp_dir().join(format!(
            "scanbus-test-bus-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("cannot create the bus's socket directory");

        let config_path = dir.join("session.conf");
        std::fs::write(
            &config_path,
            CONFIG.replace("SOCKET_DIR", &dir.to_string_lossy()),
        )
        .expect("cannot write the bus configuration");

        let mut child = match Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config_path)
            .args(["--print-address", "--nofork"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_dir_all(&dir);
                return None;
            }
            Err(error) => panic!("cannot start dbus-daemon: {error}"),
        };

        let stdout = child.stdout.take().expect("stdout was piped");
        let mut address = String::new();
        BufReader::new(stdout)
            .read_line(&mut address)
            .await
            .expect("cannot read the bus address");
        let address = address.trim().to_owned();
        assert!(
            !address.is_empty(),
            "dbus-daemon exited before printing an address"
        );

        Some(Self {
            address,
            dir,
            _child: child,
        })
    }

    /// A fresh connection to this bus, owning no name.
    pub async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .expect("the bus printed an address zbus cannot parse")
            .build()
            .await
            .expect("cannot connect to the private bus")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Prints why a test did nothing, so a skip is visible in `cargo test -- --nocapture`
/// instead of looking like a pass.
pub fn skipped(test: &str) {
    eprintln!("skipped {test}: dbus-daemon is not installed");
}
