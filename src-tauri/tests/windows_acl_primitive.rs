//! Exercise the Windows ACL primitive without loading application settings.
#![cfg(windows)]

use std::{fs, path::Path, process::Command};

use print_bridge_lib::windows_acl::{protect_data_dir_for_user, protect_file_for_user};

const SDDL_SCRIPT: &str = "if ([System.IO.Directory]::Exists($env:GOURMELYPRINT_ACL_TEST_PATH)) { $acl = [System.IO.Directory]::GetAccessControl($env:GOURMELYPRINT_ACL_TEST_PATH) } else { $acl = [System.IO.File]::GetAccessControl($env:GOURMELYPRINT_ACL_TEST_PATH) }; $acl.GetSecurityDescriptorSddlForm([System.Security.AccessControl.AccessControlSections]::All)";

fn powershell(script: &str, path: &Path) -> String {
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("GOURMELYPRINT_ACL_TEST_PATH", path)
        .output()
        .expect("start PowerShell ACL inspection");
    assert!(
        output.status.success(),
        "PowerShell failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn sddl(path: &Path) -> String {
    powershell(SDDL_SCRIPT, path)
}

#[test]
fn private_dacl_covers_legacy_files_and_owner_authority() {
    let dir = std::env::temp_dir().join(format!("gourmely-acl-unit-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).expect("create fixture directory");
    let config = dir.join("config.json");
    let bytes = b"legacy credential fixture";
    fs::write(&config, bytes).expect("write fixture");
    let sid = powershell(
        "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
        &dir,
    );
    let legacy = Command::new("icacls.exe")
        .arg(&dir)
        .args(["/grant", "*S-1-5-32-545:(OI)(CI)R"])
        .output()
        .expect("grant legacy Users read");
    assert!(legacy.status.success());

    protect_data_dir_for_user(&dir, &sid).expect("protect legacy fixture");
    for path in [&dir, &config] {
        let sddl = sddl(path);
        assert!(sddl.contains("D:P"), "DACL inherits: {sddl}");
        assert!(
            sddl.contains("RC;;;OW)"),
            "owner still has WRITE_DAC: {sddl}"
        );
        assert!(!sddl.contains(";;;BU)"), "Users retain read access: {sddl}");
    }
    assert_eq!(fs::read(&config).unwrap(), bytes);
    fs::remove_dir_all(&dir).expect("remove fixture");
}

#[test]
fn protected_credentials_reject_a_different_interactive_user_before_any_acl_change() {
    let dir = std::env::temp_dir().join(format!("gourmely-acl-identity-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).expect("create fixture directory");
    let cashier = powershell(
        "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
        &dir,
    );
    let other_user = "S-1-5-21-11111111-22222222-33333333-4444";
    assert_ne!(cashier, other_user);
    let files = [
        "config.json",
        "local-print-intents.jsonl",
        "print-jobs.jsonl.tmp",
    ];
    for name in files {
        fs::write(dir.join(name), name.as_bytes()).expect("write sensitive fixture");
    }
    protect_data_dir_for_user(&dir, &cashier).expect("first migration");
    let paths = [
        dir.clone(),
        dir.join(files[0]),
        dir.join(files[1]),
        dir.join(files[2]),
    ];
    let before: Vec<_> = paths.iter().map(|path| sddl(path)).collect();
    let error = protect_data_dir_for_user(&dir, other_user)
        .expect_err("another active Windows user must not take over protected credentials");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("another Windows user"));
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(sddl(path), expected, "ACL changed: {}", path.display());
    }
    for name in files {
        assert_eq!(fs::read(dir.join(name)).unwrap(), name.as_bytes());
    }
    protect_data_dir_for_user(&dir, &cashier).expect("same cashier remains supported");
    fs::remove_dir_all(&dir).expect("remove fixture");

    // A legacy parent may contain a protected config after an interrupted
    // migration. The guard must inspect children before touching the parent.
    let partial =
        std::env::temp_dir().join(format!("gourmely-acl-partial-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&partial).expect("create partial fixture directory");
    let config = partial.join("config.json");
    fs::write(&config, b"legacy credential").expect("write partial fixture");
    protect_file_for_user(&config, &cashier).expect("protect only config");
    let parent_before = sddl(&partial);
    let config_before = sddl(&config);
    protect_data_dir_for_user(&partial, other_user)
        .expect_err("protected child must block parent ACL takeover");
    assert_eq!(sddl(&partial), parent_before);
    assert_eq!(sddl(&config), config_before);
    assert_eq!(fs::read(&config).unwrap(), b"legacy credential");
    fs::remove_dir_all(&partial).expect("remove partial fixture");
}
