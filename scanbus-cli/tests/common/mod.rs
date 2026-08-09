//! A `dbus-daemon` of our own, and the binary under test pointed at it with `--bus`.
//!
//! The CLI's tests need something [`scanbus-daemon`'s own suite does not: a bus with no
//! daemon on it. "The daemon is not there" is a state [`scanbus-cli.md`] §3 gives its
//! own exit code, and there is no way to observe it on the developer's session bus
//! without stopping their daemon. So the bus is private, started here, and what owns
//! `org.scanbus` on it — nothing, an activation file, or a stand-in interface served by
//! the test itself — is what each case varies.
//!
//! The stand-in is also why this crate does not take `scanbus-daemon` as a
//! dev-dependency: the direction of §2 is `scanbus-cli` → `scanbus-client`, and a CLI
//! whose tests build the service is one that cannot be released without it. Driving the
//! *real* daemon from these tests is [8.11], which reuses the harness of [2.8] rather
//! than this one.
//!
//! [8.11]: https://github.com/jeanparpaillon/scanbus_service/issues/39
//! [2.8]: https://github.com/jeanparpaillon/scanbus_service/issues/12
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the harness"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// Minimal session-bus policy, plus a service directory so that activation can be
/// tested without one being wired into the developer's real session.
///
/// `receive_sender` matters as much as the other two rules: without it `dbus-daemon`
/// denies the method *returns*, and every call hangs to its timeout — which looks
/// exactly like a daemon that is not answering, i.e. like the thing under test.
const CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC
 "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=SOCKET_DIR</listen>
  <servicedir>SERVICE_DIR</servicedir>
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
    child: Child,
}

impl PrivateBus {
    /// Starts a bus with nothing on it, or `None` when `dbus-daemon` is not installed.
    ///
    /// Skipping rather than failing is the deal with CI, and the same one
    /// `scanbus-daemon`'s harness makes: a D-Bus daemon is not a build dependency of
    /// this workspace, and a runner without one should not turn a green suite red.
    pub fn start() -> Option<Self> {
        Self::start_with_activation(false)
    }

    /// Starts a bus that knows how to activate `org.scanbus` — and cannot.
    ///
    /// The activation file points at `/bin/false`, which is deliberate: `status` must
    /// report `activatable` **without** starting anything, so a service that could
    /// actually start would let a `status` that wrongly activates pass unnoticed. What
    /// happens when activation is attempted and fails is a separate case, and belongs to
    /// the first command that makes a call ([8.5]).
    ///
    /// [8.5]: https://github.com/jeanparpaillon/scanbus_service/issues/32
    pub fn start_with_activation(activatable: bool) -> Option<Self> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let dir = std::env::temp_dir().join(format!(
            "scanbus-cli-test-bus-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let services = dir.join("services");
        std::fs::create_dir_all(&services).expect("cannot create the bus's directories");

        if activatable {
            std::fs::write(
                services.join("org.scanbus.service"),
                "[D-BUS Service]\nName=org.scanbus\nExec=/bin/false\n",
            )
            .expect("cannot write the activation file");
        }

        let config_path = dir.join("session.conf");
        std::fs::write(
            &config_path,
            CONFIG
                .replace("SOCKET_DIR", &dir.to_string_lossy())
                .replace("SERVICE_DIR", &services.to_string_lossy()),
        )
        .expect("cannot write the bus configuration");

        let mut child = match Command::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address")
            .arg(format!("--config-file={}", config_path.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
            .expect("dbus-daemon printed no address");

        Some(Self {
            address: address.trim_end().to_owned(),
            dir,
            child,
        })
    }

    /// The address to hand to `--bus`.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Runs the binary under test against this bus.
    ///
    /// `--bus` is always passed, and the environment is left alone otherwise: a test
    /// that leaked the developer's `DBUS_SESSION_BUS_ADDRESS` into the child would pass
    /// or fail depending on whether they happen to be running a daemon.
    pub fn scanbus(&self, args: &[&str]) -> Run {
        let mut all = vec!["--bus", self.address()];
        all.extend_from_slice(args);
        scanbus(&all)
    }
}

/// Runs the binary under test with no bus of any kind.
///
/// For the cases that never reach a connection — a usage error, `--help` — where
/// requiring a private bus would make the test skip on a machine without
/// `dbus-daemon` for a reason that has nothing to do with what it asserts.
pub fn scanbus(args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_scanbus"))
        .args(args)
        // Colour is decided from the process's own stdout, and a piped stdout is what
        // `Command::output` gives it — which is the "not a TTY" branch of §3.
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

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One run of the binary: the three things §10 says a test may assert on.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// The exit code, with both streams in the panic message — a bare
    /// `assert_eq!(code, 3)` tells you nothing about *why* it was 1.
    #[track_caller]
    pub fn assert_code(&self, expected: i32) -> &Self {
        assert_eq!(
            self.code, expected,
            "exit {} instead of {expected}\nstdout: {}\nstderr: {}",
            self.code, self.stdout, self.stderr
        );
        self
    }

    /// stdout as JSON, which is what `--json` promises a script.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {}", self.stdout))
    }

    /// Whether either stream carries an ANSI escape.
    pub fn has_escapes(&self) -> bool {
        self.stdout.contains('\x1b') || self.stderr.contains('\x1b')
    }
}

/// Prints why a test did nothing, so a skipped run is visible in `cargo test -- --nocapture`.
pub fn skipped(test: &str) {
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "skipping {test}: dbus-daemon is not installed");
}
