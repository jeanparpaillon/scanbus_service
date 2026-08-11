use scanbus_client::ScanbusError;

pub fn present(error: &ScanbusError) -> &'static str {
    match error {
        ScanbusError::NotReachable(_) => "Scanner is not reachable",
        ScanbusError::AlreadyPaired(_) => "Scanner is already paired",
        ScanbusError::NotPaired(_) => "Scanner is not paired",
        ScanbusError::NotConnected(_) => "Scanner is not connected",
        ScanbusError::BackendInstallFailed(_) => "Backend installation failed",
        ScanbusError::UnsupportedProfile(_) => "This profile is not supported by the scanner",
        ScanbusError::Busy(_) => "Scanner is busy",
        ScanbusError::Other { .. } => "Scanbus reported an unexpected error",
    }
}
