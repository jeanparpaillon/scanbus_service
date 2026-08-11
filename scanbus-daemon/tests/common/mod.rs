//! A `dbus-daemon` of our own, and a daemon on it, for tests that need a real bus.
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

// Cargo compiles this module once per test binary, so whatever only one of them needs is
// dead code in the others — `Daemon` in `object_tree`, which brings the object server up
// by hand to assert about a name that is *not* taken. Splitting it up to silence that
// would put the daemon's startup order back into three files, which is the thing the
// module exists to stop.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use scanbus_daemon::dbus::{self, Manager1, ObjectRegistry, Profile1, path};
use scanbus_daemon::discovery::watch_owners;
use scanbus_daemon::{Backends, Discovery, MemoryPairingStore, ProfileRegistry, ScannerRegistry};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

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

    /// The address this bus is listening on.
    ///
    /// What `--bus ADDRESS` is for ([`scanbus-cli.md`] §3): a test that drives the
    /// client's own connection helper has to be able to point it here.
    ///
    /// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
    pub fn address(&self) -> &str {
        &self.address
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

/// The service side: what `main.rs` wires up, with backends a test chose.
///
/// Shared rather than repeated per test file because the *order* is the contract 2.1
/// pins down — objects exported, then the name requested — and a copy that got it wrong
/// would pass its own tests while testing a daemon nobody ships.
pub struct Daemon {
    /// The daemon's own connection, for a test that needs to act as the service.
    pub connection: zbus::Connection,
    /// The registry that decides which scanners have objects, and why.
    pub scanners: Arc<ScannerRegistry>,
    /// The discovery session `StartDiscovery` drives.
    pub discovery: Arc<Discovery>,
    /// The object tree.
    pub objects: Arc<ObjectRegistry>,
    /// Where pairings are persisted — in memory, so nothing outlives the test.
    pub store: Arc<MemoryPairingStore>,
    /// Watches `NameOwnerChanged` so a test can kill a client's connection and see its
    /// discovery reference dropped, same as [`2.9`](https://github.com/jeanparpaillon/scanbus_service/issues/34).
    owner_watch: JoinHandle<()>,
}

impl Daemon {
    /// Brings the daemon up on `bus`, in the order `main.rs` uses.
    pub async fn start(bus: &PrivateBus, backends: Backends) -> Self {
        let connection = bus.connect().await;
        let objects = Arc::new(ObjectRegistry::new(connection.clone()).await.unwrap());
        let profiles = Arc::new(ProfileRegistry::ephemeral());
        let store = Arc::new(MemoryPairingStore::new());
        let scanners = ScannerRegistry::new(
            Arc::clone(&objects),
            Arc::clone(&store) as _,
            Arc::clone(&profiles),
        );
        let discovery = Arc::new(Discovery::new(backends, Arc::clone(&scanners)));

        for kind in profiles.registered_profiles() {
            objects
                .add(
                    path::profile(kind),
                    Profile1::new(kind, Arc::clone(&profiles)),
                )
                .await
                .unwrap();
        }

        objects
            .add(
                path::manager(),
                Manager1::new(Arc::clone(&discovery), Arc::clone(&profiles)),
            )
            .await
            .unwrap();
        dbus::request_name(&connection).await.unwrap();
        let owner_watch = watch_owners(Arc::clone(&discovery), connection.clone());

        Self {
            connection,
            scanners,
            discovery,
            objects,
            store,
            owner_watch,
        }
    }

    /// Stops the discovery session and unexports the tree.
    pub async fn shutdown(&self) {
        self.owner_watch.abort();
        self.discovery.stop().await;
        self.objects.shutdown().await;
    }
}

/// Prints why a test did nothing, so a skip is visible in `cargo test -- --nocapture`
/// instead of looking like a pass.
pub fn skipped(test: &str) {
    eprintln!("skipped {test}: dbus-daemon is not installed");
}
