//! Explicit ACL repair authorization, separate from ordinary bridge startup.

#[cfg(any(windows, test))]
use std::io;

#[cfg(any(windows, test))]
pub(crate) fn authorize_repair(target: &str, active: &str, elevated: bool) -> io::Result<()> {
    if !elevated {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "ACL repair requires an elevated administrator process",
        ));
    }
    if target.is_empty() || target == "S-1-5-18" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ACL repair requires a cashier Windows user account, not SYSTEM",
        ));
    }
    if target != active {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target account is not the uniquely active cashier; sign in as that user and retry",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::authorize_repair;

    #[test]
    fn only_an_elevated_process_with_the_active_cashier_sid_may_repair() {
        let target = "S-1-5-21-100-200-300-400";
        assert!(authorize_repair(target, target, true).is_ok());
        assert!(authorize_repair(target, target, false).is_err());
        assert!(authorize_repair(target, "S-1-5-21-100-200-300-401", true).is_err());
        assert!(authorize_repair("", target, true).is_err());
        assert!(authorize_repair("S-1-5-18", "S-1-5-18", true).is_err());
    }
}
