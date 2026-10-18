//! Explicit ACL repair authorization, separate from ordinary bridge startup.

use std::ffi::{OsStr, OsString};
#[cfg(any(windows, test))]
use std::{
    fs::{File, OpenOptions, TryLockError},
    io,
    path::Path,
};

#[cfg(any(windows, test))]
pub(crate) fn hold_existing_ledger_lock(path: &Path) -> io::Result<Option<File>> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            "Bridge still holds a print ledger lock; fully stop it before ACL repair",
        ),
        TryLockError::Error(error) => error,
    })?;
    Ok(Some(file))
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartupAction {
    Normal,
    Repair(String),
}

/// Only the exact repair subcommand diverts startup. Tauri's existing command
/// line is otherwise passed through unchanged.
pub fn parse_startup_action(args: &[OsString]) -> Result<StartupAction, String> {
    if args.first().map(OsString::as_os_str) != Some(OsStr::new("repair-acl")) {
        return Ok(StartupAction::Normal);
    }
    if args.len() != 3 || args[1] != OsStr::new("--account") {
        return Err("usage: print-bridge repair-acl --account <cashier-user>".into());
    }
    let account = args[2]
        .to_str()
        .ok_or("cashier account must be valid Unicode")?
        .trim();
    if account.is_empty() {
        return Err("cashier account must not be empty".into());
    }
    Ok(StartupAction::Repair(account.to_owned()))
}

/// Return `true` only when repair ran successfully; normal launch stays in
/// main, so an invalid repair command can never start the printing service.
pub fn execute_startup_action(action: StartupAction) -> Result<bool, String> {
    match action {
        StartupAction::Normal => Ok(false),
        StartupAction::Repair(account) => {
            #[cfg(windows)]
            {
                let dir = crate::config::data_dir().ok_or("ProgramData directory unavailable")?;
                crate::windows_acl::repair_data_dir_for_account(&dir, &account)
                    .map_err(|error| format!("ACL repair failed: {error}"))?;
                Ok(true)
            }
            #[cfg(not(windows))]
            {
                let _ = account;
                Err("ACL repair is available only on Windows".into())
            }
        }
    }
}

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
    use super::{authorize_repair, hold_existing_ledger_lock, parse_startup_action, StartupAction};
    use std::ffi::OsString;

    #[test]
    fn only_an_elevated_process_with_the_active_cashier_sid_may_repair() {
        let target = "S-1-5-21-100-200-300-400";
        assert!(authorize_repair(target, target, true).is_ok());
        assert!(authorize_repair(target, target, false).is_err());
        assert!(authorize_repair(target, "S-1-5-21-100-200-300-401", true).is_err());
        assert!(authorize_repair("", target, true).is_err());
        assert!(authorize_repair("S-1-5-18", "S-1-5-18", true).is_err());
    }

    #[test]
    fn repair_command_requires_an_explicit_cashier_and_preserves_other_tauri_arguments() {
        let args = |parts: &[&str]| parts.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            parse_startup_action(&args(&[])).unwrap(),
            StartupAction::Normal
        );
        assert_eq!(
            parse_startup_action(&args(&["--tauri-internal", "value"])).unwrap(),
            StartupAction::Normal
        );
        assert_eq!(
            parse_startup_action(&args(&["repair-acl", "--account", "STORE\\cashier"])).unwrap(),
            StartupAction::Repair("STORE\\cashier".to_owned())
        );
        for bad in [
            args(&["repair-acl"]),
            args(&["repair-acl", "--account"]),
            args(&["repair-acl", "--account", ""]),
            args(&["repair-acl", "--account", "cashier", "--unexpected"]),
        ] {
            assert!(parse_startup_action(&bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn active_ledger_lock_refuses_repair_until_the_bridge_stops() {
        let dir = std::env::temp_dir().join(format!("bridge-repair-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("local-print-intents.lock");
        let active = crate::instance_lock::InstanceLock::acquire(&path).unwrap();
        assert!(hold_existing_ledger_lock(&path).is_err());
        drop(active);
        assert!(hold_existing_ledger_lock(&path).unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
