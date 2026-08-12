use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

pub const DESKTOP_FILE_NAME: &str = "org.scanbus.Gui.desktop";

pub fn is_enabled() -> io::Result<bool> {
    let path = user_override_path();
    if !path.exists() {
        return Ok(true);
    }

    let desktop = fs::read_to_string(path)?;
    Ok(!is_hidden_override(&desktop))
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let path = user_override_path();
    if enabled {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, disabled_override())
    }
}

fn user_override_path() -> PathBuf {
    config_home().join("autostart").join(DESKTOP_FILE_NAME)
}

fn config_home() -> PathBuf {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path);
    }

    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }

    PathBuf::from(".config")
}

fn disabled_override() -> &'static str {
    "[Desktop Entry]\nType=Application\nName=Scanbus\nHidden=true\n"
}

fn is_hidden_override(desktop: &str) -> bool {
    desktop.lines().any(|line| {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        key.trim() == "Hidden" && value.trim().eq_ignore_ascii_case("true")
    })
}

#[cfg(test)]
mod tests {
    use super::is_hidden_override;

    #[test]
    fn hidden_override_is_detected() {
        assert!(is_hidden_override(
            "[Desktop Entry]\nType=Application\nHidden=true\n"
        ));
    }

    #[test]
    fn missing_hidden_key_leaves_autostart_enabled() {
        assert!(!is_hidden_override("[Desktop Entry]\nType=Application\n"));
    }
}
