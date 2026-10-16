//! Exercise the Windows ACL primitive without loading application settings.
#![cfg(windows)]

use std::{fs, path::Path, process::Command};

use print_bridge_lib::windows_acl::protect_data_dir_for_user;

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
        let sddl = powershell(
            "if ([System.IO.Directory]::Exists($env:GOURMELYPRINT_ACL_TEST_PATH)) { $acl = [System.IO.Directory]::GetAccessControl($env:GOURMELYPRINT_ACL_TEST_PATH) } else { $acl = [System.IO.File]::GetAccessControl($env:GOURMELYPRINT_ACL_TEST_PATH) }; $acl.GetSecurityDescriptorSddlForm([System.Security.AccessControl.AccessControlSections]::All)",
            path,
        );
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
