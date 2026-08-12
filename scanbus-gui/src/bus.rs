use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::thread;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use futures_util::StreamExt as _;
use scanbus_client::profile::is_unset;
use scanbus_client::proxy::{
    Button1Proxy, Manager1Proxy, Profile1Proxy, Scanner1Proxy, object_manager,
};
use scanbus_client::{
    Bus, Error, OptionsSchema, Presence, ScanbusError, connect, convert, presence,
};
use scanbus_core::{ProfileKind, path};
use tokio::runtime::Builder;
use tokio::select;
use tracing::{debug, warn};
use zbus::message::Type;
use zbus::zvariant::OwnedValue;
use zbus::{Message, MessageStream};

use crate::scanners::ToastSpec;
use crate::store::{Dict, ManagedSnapshot, ServiceDetails, ServiceState, StoreEvent};

#[derive(Debug, Clone)]
pub enum BusCommand {
    StartService,
    StartDiscovery,
    StopDiscovery {
        quiet: bool,
    },
    Connect {
        path: String,
    },
    Disconnect {
        path: String,
    },
    Pair {
        path: String,
    },
    CancelPairing {
        path: String,
    },
    Unpair {
        path: String,
    },
    /// `Button1.Profile` on a button object path; `None` writes `""` — unassigned (§5).
    SetButtonProfile {
        path: String,
        kind: Option<ProfileKind>,
    },
    /// `Button1.Label` on a button object path. Only ever sent for a button whose
    /// `LabelConfigurable` is true — §9 is explicit that offering the rename otherwise
    /// is offering an action that silently fails.
    SetButtonLabel {
        path: String,
        label: String,
    },
    /// `Scanner1.DefaultProfile` on a scanner object path; `None` writes `""`.
    SetDefaultProfile {
        path: String,
        kind: Option<ProfileKind>,
    },
    /// `Profile1.Options` on a profile object path.
    SetProfileOptions {
        kind: ProfileKind,
        options: std::collections::BTreeMap<String, scanbus_core::Value>,
    },
    /// `Button1.ProfileOptions` on a button object path.
    SetButtonProfileOptions {
        path: String,
        options: std::collections::BTreeMap<String, scanbus_core::Value>,
    },
}

#[derive(Debug, Clone)]
pub enum BusEvent {
    Store(StoreEvent),
    DiscoveryActive(bool),
    Toast(ToastSpec),
}

#[derive(Clone)]
pub struct BusHandle {
    commands: Sender<BusCommand>,
    events: Receiver<BusEvent>,
}

impl BusHandle {
    pub fn start(bus: Bus) -> Self {
        let (commands_tx, commands_rx) = async_channel::unbounded();
        let (events_tx, events_rx) = async_channel::unbounded();

        thread::spawn(move || {
            let runtime = Builder::new_multi_thread()
                .enable_all()
                .thread_name("scanbus-gui-bus")
                .build()
                .expect("tokio runtime creation should succeed");

            runtime.block_on(run(bus, commands_rx, events_tx));
        });

        Self {
            commands: commands_tx,
            events: events_rx,
        }
    }

    pub fn commands(&self) -> Sender<BusCommand> {
        self.commands.clone()
    }

    pub fn events(&self) -> Receiver<BusEvent> {
        self.events.clone()
    }
}

async fn run(bus: Bus, commands: Receiver<BusCommand>, events: Sender<BusEvent>) {
    loop {
        match run_session(&bus, &commands, &events).await {
            Ok(()) => return,
            Err(error) => {
                warn!(%error, "scanbus-gui bus loop failed; retrying");
                let _ = events
                    .send(BusEvent::Store(StoreEvent::ServiceState(
                        ServiceState::Unknown,
                    )))
                    .await;
                let _ = events.send(BusEvent::DiscoveryActive(false)).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn run_session(
    bus: &Bus,
    commands: &Receiver<BusCommand>,
    events: &Sender<BusEvent>,
) -> Result<(), Error> {
    let connection = bus.connect().await?;
    let mut leases = DiscoveryLeases::default();
    let mut messages = MessageStream::from(&connection);

    loop {
        match presence(&connection).await? {
            Presence::Running => {}
            Presence::Activatable => {
                let _ = events
                    .send(BusEvent::Store(StoreEvent::ServiceState(
                        ServiceState::Activatable,
                    )))
                    .await;
                let _ = events.send(BusEvent::DiscoveryActive(false)).await;
                if !wait_for_owner(bus, &connection, commands, events, &mut messages).await? {
                    return Ok(());
                }
                continue;
            }
            Presence::Absent => {
                let _ = events
                    .send(BusEvent::Store(StoreEvent::ServiceState(
                        ServiceState::Absent,
                    )))
                    .await;
                let _ = events.send(BusEvent::DiscoveryActive(false)).await;
                if !wait_for_owner(bus, &connection, commands, events, &mut messages).await? {
                    return Ok(());
                }
                continue;
            }
        }

        debug!("scanbus owner appeared");
        let manager = object_manager(&connection).await?;
        let managed = manager.get_managed_objects().await?;
        let details = load_service_details(&connection).await?;
        let _ = events
            .send(BusEvent::Store(StoreEvent::ServiceState(
                ServiceState::Running,
            )))
            .await;
        let _ = events
            .send(BusEvent::Store(StoreEvent::Replace(normalize_snapshot(
                managed,
            ))))
            .await;
        let _ = events
            .send(BusEvent::Store(StoreEvent::ServiceDetails(details)))
            .await;

        loop {
            select! {
                command = commands.recv() => {
                    let Ok(command) = command else {
                        return Ok(());
                    };
                    // A refusal has already become a toast; only the rest is a surprise.
                    if let Err(error) =
                        handle_command(bus, &connection, command, events, &mut leases).await
                        && !matches!(error, Error::Call(_))
                    {
                        warn!(%error, "GUI command failed");
                    }
                }
                message = messages.next() => {
                    let Some(message) = message else {
                        return Ok(());
                    };
                    if dispatch_signal(message?, events, &mut leases).await? {
                        break;
                    }
                }
            }
        }
    }
}

async fn wait_for_owner(
    _bus: &Bus,
    connection: &zbus::Connection,
    commands: &Receiver<BusCommand>,
    events: &Sender<BusEvent>,
    messages: &mut MessageStream,
) -> Result<bool, Error> {
    loop {
        select! {
            command = commands.recv() => {
                let Ok(command) = command else {
                    return Ok(false);
                };
                if let BusCommand::StartService = command
                    && let Err(error) = start_service(connection, events).await
                {
                    warn!(%error, "could not start the scanbus service");
                }
            }
            message = messages.next() => {
                let Some(message) = message else {
                    return Ok(false);
                };
                let message = message?;
                if is_signal(&message, "org.freedesktop.DBus", "NameOwnerChanged")
                    && let Ok((name, _old_owner, new_owner)) =
                        message.body().deserialize::<(String, String, String)>()
                    && name == scanbus_client::BUS_NAME
                    && !new_owner.is_empty()
                {
                    return Ok(true);
                }
            }
        }
    }
}

async fn dispatch_signal(
    message: Message,
    events: &Sender<BusEvent>,
    leases: &mut DiscoveryLeases,
) -> Result<bool, Error> {
    if message.message_type() != Type::Signal {
        return Ok(false);
    }

    let header = message.header();
    let Some(interface) = header.interface().map(|name| name.as_str().to_owned()) else {
        return Ok(false);
    };
    let Some(member) = header.member().map(|name| name.as_str().to_owned()) else {
        return Ok(false);
    };

    if interface == "org.freedesktop.DBus"
        && member == "NameOwnerChanged"
        && let Ok((name, _old_owner, new_owner)) =
            message.body().deserialize::<(String, String, String)>()
        && name == scanbus_client::BUS_NAME
    {
        if new_owner.is_empty() {
            leases.clear();
            let _ = events
                .send(BusEvent::Store(StoreEvent::ServiceState(
                    ServiceState::Unknown,
                )))
                .await;
            let _ = events.send(BusEvent::DiscoveryActive(false)).await;
            return Ok(true);
        }
        return Ok(true);
    }

    let path = header
        .path()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_default();
    if !path.starts_with(path::ROOT) {
        return Ok(false);
    }

    if interface == "org.freedesktop.DBus.ObjectManager" && member == "InterfacesAdded" {
        let (path, interfaces) = message
            .body()
            .deserialize::<(String, HashMap<String, HashMap<String, OwnedValue>>)>()?;
        let _ = events
            .send(BusEvent::Store(StoreEvent::InterfacesAdded {
                path,
                interfaces,
            }))
            .await;
    } else if interface == "org.freedesktop.DBus.ObjectManager" && member == "InterfacesRemoved" {
        let (path, interfaces) = message.body().deserialize::<(String, Vec<String>)>()?;
        let _ = events
            .send(BusEvent::Store(StoreEvent::InterfacesRemoved {
                path,
                interfaces,
            }))
            .await;
    } else if interface == "org.freedesktop.DBus.Properties" && member == "PropertiesChanged" {
        let (iface, changed, invalidated) = message
            .body()
            .deserialize::<(String, Dict, Vec<String>)>()?;
        leases
            .release_finished_pairing(&path, &iface, &changed)
            .await?;
        let _ = events
            .send(BusEvent::Store(StoreEvent::PropertiesChanged {
                path,
                interface: iface,
                changed,
                invalidated,
            }))
            .await;
    }

    Ok(false)
}

fn is_signal(message: &Message, interface: &str, member: &str) -> bool {
    message.message_type() == Type::Signal
        && message
            .header()
            .interface()
            .is_some_and(|name| name.as_str() == interface)
        && message
            .header()
            .member()
            .is_some_and(|name| name.as_str() == member)
}

fn normalize_snapshot(managed: zbus::fdo::ManagedObjects) -> ManagedSnapshot {
    managed
        .into_iter()
        .map(|(path, interfaces)| {
            (
                path.as_str().to_owned(),
                interfaces
                    .into_iter()
                    .map(|(name, properties)| (name.as_str().to_owned(), properties))
                    .collect(),
            )
        })
        .collect()
}

async fn handle_command(
    bus: &Bus,
    connection: &zbus::Connection,
    command: BusCommand,
    events: &Sender<BusEvent>,
    leases: &mut DiscoveryLeases,
) -> Result<(), Error> {
    match command {
        BusCommand::StartService => start_service(connection, events).await?,
        BusCommand::StartDiscovery => {
            let manager = Manager1Proxy::new(connection).await?;
            match manager.start_discovery(HashMap::new()).await {
                Ok(()) => {
                    let _ = events.send(BusEvent::DiscoveryActive(true)).await;
                }
                Err(error) => {
                    let error = Error::from(error);
                    let _ = events.send(BusEvent::DiscoveryActive(false)).await;
                    if let Error::Call(ref refusal) = error {
                        debug!(
                            message = %crate::error::present(refusal),
                            detail = %refusal,
                            "GUI command failed"
                        );
                        let _ = events
                            .send(BusEvent::Toast(ToastSpec::new(crate::error::present(
                                refusal,
                            ))))
                            .await;
                    }
                    return Err(error);
                }
            }
        }
        BusCommand::StopDiscovery { quiet } => {
            let manager = Manager1Proxy::new(connection).await?;
            match manager.stop_discovery().await {
                Ok(()) => {
                    let _ = events.send(BusEvent::DiscoveryActive(false)).await;
                }
                Err(error) => {
                    let error = Error::from(error);
                    let _ = events.send(BusEvent::DiscoveryActive(false)).await;
                    if !quiet && let Error::Call(ref refusal) = error {
                        let _ = events
                            .send(BusEvent::Toast(ToastSpec::new(crate::error::present(
                                refusal,
                            ))))
                            .await;
                    }
                    return Err(error);
                }
            }
        }
        BusCommand::Pair { path } => {
            if let Some(id) = path::scanner_id(&path) {
                leases.acquire_pairing_hold(bus, &path).await?;
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                match proxy.pair(HashMap::new()).await {
                    Ok(()) => {}
                    Err(error) => {
                        leases.release_pairing_hold(&path).await?;
                        let error = Error::from(error);
                        if let Error::Call(ref refusal) = error {
                            match refusal {
                                ScanbusError::AlreadyPaired(_) => return Ok(()),
                                ScanbusError::NotReachable(_) => {
                                    let _ = events
                                        .send(BusEvent::Toast(ToastSpec::retry_pair(
                                            crate::error::present(refusal),
                                            path,
                                        )))
                                        .await;
                                }
                                _ => {
                                    let _ = events
                                        .send(BusEvent::Toast(ToastSpec::new(
                                            crate::error::present(refusal),
                                        )))
                                        .await;
                                }
                            }
                        }
                        return Err(error);
                    }
                }
            }
        }
        BusCommand::Connect { path } => {
            if let Some(id) = path::scanner_id(&path) {
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                match proxy.connect(HashMap::new()).await {
                    Ok(()) => {}
                    Err(error) => {
                        let error = Error::from(error);
                        if let Error::Call(ref refusal) = error {
                            match refusal {
                                ScanbusError::NotPaired(_) | ScanbusError::NotConnected(_) => {
                                    return Ok(());
                                }
                                ScanbusError::Busy(_) | ScanbusError::NotReachable(_) => {
                                    let _ = events
                                        .send(BusEvent::Toast(ToastSpec::new(
                                            crate::error::present(refusal),
                                        )))
                                        .await;
                                }
                                _ => {
                                    let _ = events
                                        .send(BusEvent::Toast(ToastSpec::new(
                                            crate::error::present(refusal),
                                        )))
                                        .await;
                                }
                            }
                        }
                        return Err(error);
                    }
                }
            }
        }
        BusCommand::Disconnect { path } => {
            if let Some(id) = path::scanner_id(&path) {
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                match proxy.disconnect().await {
                    Ok(()) => {}
                    Err(error) => {
                        let error = Error::from(error);
                        if let Error::Call(ref refusal) = error {
                            match refusal {
                                ScanbusError::NotPaired(_) | ScanbusError::NotConnected(_) => {
                                    return Ok(());
                                }
                                ScanbusError::Busy(_) => {
                                    let _ = events
                                        .send(BusEvent::Toast(ToastSpec::new(
                                            crate::error::present(refusal),
                                        )))
                                        .await;
                                }
                                _ => {
                                    let _ = events
                                        .send(BusEvent::Toast(ToastSpec::new(
                                            crate::error::present(refusal),
                                        )))
                                        .await;
                                }
                            }
                        }
                        return Err(error);
                    }
                }
            }
        }
        BusCommand::CancelPairing { path } => {
            if let Some(id) = path::scanner_id(&path) {
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                proxy.cancel_pairing().await?;
            }
        }
        BusCommand::Unpair { path } => {
            if let Some(id) = path::scanner_id(&path) {
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                proxy.unpair().await?;
            }
        }
        BusCommand::SetButtonProfile { path, kind } => {
            if let Some((scanner, index)) = path::button_index(&path) {
                let proxy = Button1Proxy::for_button(connection, &scanner, index).await?;
                let value = ProfileKind::optional_as_str(kind);
                if let Err(error) = proxy.set_profile(value).await {
                    return Err(refused(events, error).await);
                }
            }
        }
        BusCommand::SetButtonLabel { path, label } => {
            if let Some((scanner, index)) = path::button_index(&path) {
                let proxy = Button1Proxy::for_button(connection, &scanner, index).await?;
                if let Err(error) = proxy.set_label(&label).await {
                    return Err(refused(events, error).await);
                }
            }
        }
        BusCommand::SetDefaultProfile { path, kind } => {
            if let Some(id) = path::scanner_id(&path) {
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                let value = ProfileKind::optional_as_str(kind);
                if let Err(error) = proxy.set_default_profile(value).await {
                    return Err(refused(events, error).await);
                }
            }
        }
        BusCommand::SetProfileOptions { kind, options } => {
            let proxy = Profile1Proxy::for_profile(connection, kind).await?;
            if let Err(error) = proxy.set_options(convert::to_dict(&options)).await {
                return Err(refused(events, error).await);
            }
        }
        BusCommand::SetButtonProfileOptions { path, options } => {
            if let Some((scanner, index)) = path::button_index(&path) {
                let proxy = Button1Proxy::for_button(connection, &scanner, index).await?;
                if let Err(error) = proxy.set_profile_options(convert::to_dict(&options)).await {
                    return Err(refused(events, error).await);
                }
            }
        }
    }

    Ok(())
}

async fn start_service(
    connection: &zbus::Connection,
    events: &Sender<BusEvent>,
) -> Result<(), Error> {
    if presence(connection).await? == Presence::Absent {
        let _ = events
            .send(BusEvent::Toast(ToastSpec::new(
                "Scanbus is not installed on this session bus",
            )))
            .await;
        return Ok(());
    }

    let manager = Manager1Proxy::new(connection).await?;
    if let Err(error) = manager.get_profile_types().await {
        let error = Error::from(error);
        if let Error::Call(ref refusal) = error {
            let _ = events
                .send(BusEvent::Toast(ToastSpec::new(crate::error::present(
                    refusal,
                ))))
                .await;
        }
        return Err(error);
    }
    Ok(())
}

async fn load_service_details(connection: &zbus::Connection) -> Result<ServiceDetails, Error> {
    let manager = Manager1Proxy::new(connection).await?;
    let version = manager.version().await?;
    let backends = manager.backends().await?;
    let profile_types = manager.get_profile_types().await?;
    let mut output_folders = BTreeMap::new();

    for profile in &profile_types {
        let Ok(kind) = profile.parse::<ProfileKind>() else {
            continue;
        };
        let schema = OptionsSchema::fetch(connection, kind).await?;
        let options = Profile1Proxy::for_profile(connection, kind)
            .await?
            .options()
            .await?;
        let options = convert::from_dict(&options)?;
        if let Some(entry) = schema.get("output_folder") {
            let folder = entry
                .effective(
                    options
                        .get("output_folder")
                        .filter(|value| !is_unset(value)),
                )
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned();
            output_folders.insert(kind, folder);
        }
    }

    Ok(ServiceDetails {
        version: Some(version),
        backends,
        profile_types,
        output_folders,
    })
}

/// Turns a rejected property write into a toast.
///
/// The write is all the client did — API §5 says the daemon rewrites the backend's
/// configuration and the client "only sees the updated property", so a refusal leaves the
/// store exactly as it was and the widget must go back to it. That revert is
/// [`crate::scanners::ScannerListModel::refresh`], driven from the toast in `main.rs`;
/// here we only name what went wrong.
async fn refused(events: &Sender<BusEvent>, error: zbus::Error) -> Error {
    let error = Error::from(error);
    if let Error::Call(ref refusal) = error {
        let _ = events
            .send(BusEvent::Toast(ToastSpec::new(crate::error::present(
                refusal,
            ))))
            .await;
    }
    error
}

#[derive(Default)]
struct DiscoveryLeases {
    held_paths: HashSet<String>,
    hold_connection: Option<zbus::Connection>,
}

impl DiscoveryLeases {
    async fn acquire_pairing_hold(&mut self, bus: &Bus, path: &str) -> Result<(), Error> {
        if !self.held_paths.insert(path.to_owned()) {
            return Ok(());
        }

        if self.hold_connection.is_some() {
            return Ok(());
        }

        let connection = connect(bus, true).await?;
        let manager = Manager1Proxy::new(&connection).await?;
        manager.start_discovery(HashMap::new()).await?;
        self.hold_connection = Some(connection);
        Ok(())
    }

    async fn release_pairing_hold(&mut self, path: &str) -> Result<(), Error> {
        if !self.held_paths.remove(path) {
            return Ok(());
        }

        if !self.held_paths.is_empty() {
            return Ok(());
        }

        if let Some(connection) = self.hold_connection.take() {
            let manager = Manager1Proxy::new(&connection).await?;
            manager.stop_discovery().await?;
        }

        Ok(())
    }

    async fn release_finished_pairing(
        &mut self,
        path: &str,
        interface: &str,
        changed: &Dict,
    ) -> Result<(), Error> {
        if interface != scanbus_client::proxy::SCANNER_INTERFACE {
            return Ok(());
        }

        let Some(value) = changed.get("PairingState") else {
            return Ok(());
        };

        let state = value
            .try_clone()
            .ok()
            .and_then(|value| String::try_from(value).ok());
        let in_progress = matches!(
            state.as_deref(),
            Some("pairing" | "installing_backend" | "awaiting_confirmation")
        );

        if !in_progress {
            self.release_pairing_hold(path).await?;
        }

        Ok(())
    }

    fn clear(&mut self) {
        self.held_paths.clear();
        self.hold_connection = None;
    }
}
