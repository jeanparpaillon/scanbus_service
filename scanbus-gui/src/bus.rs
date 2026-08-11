use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use futures_util::StreamExt as _;
use scanbus_client::proxy::{Button1Proxy, Scanner1Proxy, object_manager};
use scanbus_client::{Bus, Error, connect, owner};
use scanbus_core::{ProfileKind, path};
use tokio::runtime::Builder;
use tokio::select;
use tracing::{debug, warn};
use zbus::message::Type;
use zbus::zvariant::OwnedValue;
use zbus::{Message, MessageStream};

use crate::store::{Dict, ManagedSnapshot, StoreEvent};

#[derive(Debug, Clone)]
pub enum BusCommand {
    Pair {
        path: String,
    },
    SetProfile {
        path: String,
        kind: Option<ProfileKind>,
    },
}

#[derive(Clone)]
pub struct BusHandle {
    commands: Sender<BusCommand>,
    events: Receiver<StoreEvent>,
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

    pub fn events(&self) -> Receiver<StoreEvent> {
        self.events.clone()
    }
}

async fn run(bus: Bus, commands: Receiver<BusCommand>, events: Sender<StoreEvent>) {
    loop {
        match run_session(&bus, &commands, &events).await {
            Ok(()) => return,
            Err(error) => {
                warn!(%error, "scanbus-gui bus loop failed; retrying");
                let _ = events.send(StoreEvent::ServicePresent(false)).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn run_session(
    bus: &Bus,
    commands: &Receiver<BusCommand>,
    events: &Sender<StoreEvent>,
) -> Result<(), Error> {
    let connection = connect(bus, true).await?;
    let mut messages = MessageStream::from(&connection);

    loop {
        let Some(current_owner) = owner(&connection).await? else {
            let _ = events.send(StoreEvent::ServicePresent(false)).await;
            wait_for_owner(&mut messages).await?;
            continue;
        };

        debug!(owner = %current_owner, "scanbus owner appeared");
        let manager = object_manager(&connection).await?;
        let managed = manager.get_managed_objects().await?;
        let _ = events.send(StoreEvent::ServicePresent(true)).await;
        let _ = events
            .send(StoreEvent::Replace(normalize_snapshot(managed)))
            .await;

        loop {
            select! {
                command = commands.recv() => {
                    let Ok(command) = command else {
                        return Ok(());
                    };
                    if let Err(error) = handle_command(&connection, command).await {
                        if let Error::Call(ref refusal) = error {
                            debug!(message = %crate::error::present(refusal), detail = %refusal, "GUI command failed");
                        } else {
                            warn!(%error, "GUI command failed");
                        }
                    }
                }
                message = messages.next() => {
                    let Some(message) = message else {
                        return Ok(());
                    };
                    if dispatch_signal(message?, events).await? {
                        break;
                    }
                }
            }
        }
    }
}

async fn wait_for_owner(messages: &mut MessageStream) -> Result<(), Error> {
    while let Some(message) = messages.next().await {
        let message = message?;
        if is_signal(&message, "org.freedesktop.DBus", "NameOwnerChanged")
            && let Ok((name, _old_owner, new_owner)) =
                message.body().deserialize::<(String, String, String)>()
            && name == scanbus_client::BUS_NAME
            && !new_owner.is_empty()
        {
            return Ok(());
        }
    }

    Ok(())
}

async fn dispatch_signal(message: Message, events: &Sender<StoreEvent>) -> Result<bool, Error> {
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
            let _ = events.send(StoreEvent::ServicePresent(false)).await;
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
            .send(StoreEvent::InterfacesAdded { path, interfaces })
            .await;
    } else if interface == "org.freedesktop.DBus.ObjectManager" && member == "InterfacesRemoved" {
        let (path, interfaces) = message.body().deserialize::<(String, Vec<String>)>()?;
        let _ = events
            .send(StoreEvent::InterfacesRemoved { path, interfaces })
            .await;
    } else if interface == "org.freedesktop.DBus.Properties" && member == "PropertiesChanged" {
        let (iface, changed, invalidated) = message
            .body()
            .deserialize::<(String, Dict, Vec<String>)>()?;
        let _ = events
            .send(StoreEvent::PropertiesChanged {
                path,
                interface: iface,
                changed,
                invalidated,
            })
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

async fn handle_command(connection: &zbus::Connection, command: BusCommand) -> Result<(), Error> {
    match command {
        BusCommand::Pair { path } => {
            if let Some(id) = path::scanner_id(&path) {
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                proxy.pair(HashMap::new()).await?;
            }
        }
        BusCommand::SetProfile { path, kind } => {
            let value = kind.map_or_else(String::new, |kind| kind.to_string());

            if let Some(id) = path::scanner_id(&path) {
                let proxy = Scanner1Proxy::for_scanner(connection, &id).await?;
                proxy.set_default_profile(&value).await?;
            } else if let Some((scanner, index)) = path::button_index(&path) {
                let proxy = Button1Proxy::for_button(connection, &scanner, index).await?;
                proxy.set_profile(&value).await?;
            }
        }
    }

    Ok(())
}
