use std::sync::Arc;

use scanbus_core::ProfileKind;
use zbus::fdo;

use crate::dbus::convert::{self, Dict};
use crate::profiles::ProfileRegistry;

/// `org.scanbus.Profile1` on `/org/scanbus/profile/{name}`.
pub struct Profile1 {
    kind: ProfileKind,
    profiles: Arc<ProfileRegistry>,
}

impl Profile1 {
    pub fn new(kind: ProfileKind, profiles: Arc<ProfileRegistry>) -> Self {
        Self { kind, profiles }
    }
}

#[zbus::interface(name = "org.scanbus.Profile1")]
impl Profile1 {
    /// The profile name from this object's path.
    #[zbus(property(emits_changed_signal = "const"))]
    fn name(&self) -> String {
        self.kind.as_str().to_owned()
    }

    /// Profile defaults this daemon applies for this profile kind.
    #[zbus(property)]
    async fn options(&self) -> Dict {
        let options = self
            .profiles
            .options_for(self.kind)
            .await
            .unwrap_or_default();
        convert::dict(options.iter().map(|(key, value)| (key.clone(), value)))
    }

    /// What this profile's options *are*: type, constraints and effective default (§6).
    ///
    /// Generated from the table `Options` writes are validated against
    /// ([`crate::profiles::options`]), so the two cannot disagree — which is the whole
    /// reason a client is told to build its editor from this rather than from the prose.
    ///
    /// Read on every call rather than served from a cache: `output_folder`'s default
    /// follows the XDG user directories, and a value that goes stale silently is worse
    /// than one that costs a path resolution.
    #[zbus(property)]
    fn options_schema(&self) -> Dict {
        convert::options_schema(&self.profiles.options_schema(self.kind))
    }

    /// Replaces profile defaults, persisted across daemon restarts.
    #[zbus(property)]
    async fn set_options(&self, value: Dict) -> fdo::Result<()> {
        let options = convert::from_dict(&value)
            .map_err(|error| fdo::Error::InvalidArgs(format!("Options rejected: {error}")))?;

        self.profiles
            .set_options(self.kind, options)
            .await
            .map_err(fdo::Error::InvalidArgs)
    }
}
