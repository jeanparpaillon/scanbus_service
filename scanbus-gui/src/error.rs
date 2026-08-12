use scanbus_client::ScanbusError;

pub fn present(error: &ScanbusError) -> String {
    match error {
        ScanbusError::NotReachable(_) => "Scanner is not reachable".to_owned(),
        ScanbusError::AlreadyPaired(_) => "Scanner is already paired".to_owned(),
        ScanbusError::NotPaired(_) => "Scanner is not paired".to_owned(),
        ScanbusError::NotConnected(_) => "Scanner is not connected".to_owned(),
        ScanbusError::BackendInstallFailed(_) => "Backend installation failed".to_owned(),
        ScanbusError::UnsupportedProfile(_) => {
            "This profile is not supported by the scanner".to_owned()
        }
        ScanbusError::Busy(_) => "Scanner is busy".to_owned(),
        ScanbusError::Other { name, message }
            if name == "org.freedesktop.DBus.Error.InvalidArgs" =>
        {
            if message.is_empty() {
                "Discovery request was invalid".to_owned()
            } else {
                format!("Discovery request was invalid: {message}")
            }
        }
        ScanbusError::Other { .. } => "Scanbus reported an unexpected error".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::present;
    use scanbus_client::ScanbusError;

    #[test]
    fn invalid_args_error_keeps_the_specific_copy() {
        let error = ScanbusError::Other {
            name: "org.freedesktop.DBus.Error.InvalidArgs".to_owned(),
            message: "page size is not supported".to_owned(),
        };

        assert_eq!(
            present(&error),
            "Discovery request was invalid: page size is not supported"
        );
    }

    #[test]
    fn unknown_errors_fall_back_to_generic_copy() {
        let error = ScanbusError::Other {
            name: "org.scanbus.Error.SomethingElse".to_owned(),
            message: "internal details".to_owned(),
        };

        assert_eq!(present(&error), "Scanbus reported an unexpected error");
    }
}
