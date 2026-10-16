//! Windows-only regression for the single-user ProgramData configuration policy.
//! Run on a Windows test machine; a macOS test run cannot exercise NTFS ACLs.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use print_bridge_lib::config::SettingsStore;

struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::env::remove_var("GOURMELYPRINT_DATA_DIR");
        std::env::remove_var("GOURMELYPRINT_ACL_TEST_SID");
        let _ = fs::remove_dir_all(&self.0);
    }
}

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
    String::from_utf8(output.stdout)
        .expect("PowerShell UTF-8 output")
        .trim()
        .to_string()
}

fn grant_legacy_users_read(path: &Path, inherited: bool) {
    let grant = if inherited {
        "*S-1-5-32-545:(OI)(CI)R"
    } else {
        "*S-1-5-32-545:R"
    };
    let output = Command::new("icacls.exe")
        .arg(path)
        .args(["/grant", grant])
        .output()
        .expect("start icacls fixture setup");
    assert!(
        output.status.success(),
        "icacls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_private_acl(path: &Path, user_sid: &str, protected: bool) {
    let sddl = powershell(
        "if ([System.IO.Directory]::Exists($env:GOURMELYPRINT_ACL_TEST_PATH)) { $acl = [System.IO.Directory]::GetAccessControl($env:GOURMELYPRINT_ACL_TEST_PATH) } else { $acl = [System.IO.File]::GetAccessControl($env:GOURMELYPRINT_ACL_TEST_PATH) }; $acl.GetSecurityDescriptorSddlForm([System.Security.AccessControl.AccessControlSections]::All)",
        path,
    );
    if protected {
        assert!(sddl.contains("D:P"), "DACL still inherits: {sddl}");
    }
    let dacl = sddl.split("D:").nth(1).expect("DACL");
    let dacl = dacl.split("S:").next().unwrap_or(dacl);
    let trustees: Vec<&str> = dacl
        .split('(')
        .skip(1)
        .filter_map(|ace| ace.split(')').next())
        .filter_map(|ace| ace.rsplit(";;;").next())
        .collect();
    // OWNER RIGHTS removes the implicit WRITE_DAC held by a legacy owner.
    // Without it, an old account could silently grant itself access again.
    assert!(
        dacl.contains("(A;;RC;;;OW)") || dacl.contains("(A;OICI;RC;;;OW)"),
        "legacy owner can still rewrite the DACL: {sddl}"
    );
    // Windows serializes the local Administrator RID-500 SID as the SDDL
    // well-known alias LA, even when the ACE was created from its numeric SID.
    let expected_user = if user_sid.ends_with("-500") {
        "LA"
    } else if user_sid == "S-1-5-18" {
        "SY"
    } else {
        user_sid
    };
    assert!(!trustees.is_empty(), "empty DACL: {sddl}");
    for &trustee in &trustees {
        assert!(
            trustee == expected_user
                || matches!(trustee, "SY" | "BA" | "OW" | "S-1-5-18" | "S-1-5-32-544"),
            "unexpected ACL trustee {trustee}: {sddl}"
        );
    }
    assert!(
        trustees.contains(&expected_user),
        "current user has no explicit access: {sddl}"
    );
}

#[test]
fn migrates_legacy_config_without_exposing_token_or_losing_access() {
    let dir = std::env::temp_dir().join(format!("gourmely-acl-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).expect("create disposable data directory");
    let _sandbox = Sandbox(dir.clone());
    let config = dir.join("config.json");
    let original = br#"{"stationToken":"legacy-token","localSecret":"legacy-secret"}"#;
    fs::write(&config, original).expect("write legacy config fixture");
    let sensitive = [
        ("config.json.tmp", b"stale-secret".as_slice()),
        ("local-print-intents.jsonl", b"ledger-record\n".as_slice()),
        ("local-print-intents.lock", b"".as_slice()),
        ("print-jobs.jsonl", b"relay-record\n".as_slice()),
        ("print-jobs.jsonl.tmp", b"relay-temp".as_slice()),
        ("print-jobs.lock", b"".as_slice()),
    ];
    for (name, bytes) in sensitive {
        let path = dir.join(name);
        fs::write(&path, bytes).expect("write sensitive fixture");
        grant_legacy_users_read(&path, false);
    }
    grant_legacy_users_read(&dir, true);
    grant_legacy_users_read(&config, false);
    std::env::set_var("GOURMELYPRINT_DATA_DIR", &dir);

    let user_sid = powershell(
        "[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
        &dir,
    );
    std::env::set_var("GOURMELYPRINT_ACL_TEST_SID", &user_sid);
    let store = SettingsStore::load();
    assert_eq!(
        store.snapshot().station_token.as_deref(),
        Some("legacy-token")
    );
    assert_eq!(fs::read(&config).expect("read migrated config"), original);
    assert_private_acl(&dir, &user_sid, true);
    assert_private_acl(&config, &user_sid, true);
    for (name, bytes) in sensitive {
        let path = dir.join(name);
        assert_eq!(
            fs::read(&path).expect("read existing sensitive file"),
            bytes
        );
        assert_private_acl(&path, &user_sid, true);
    }

    store
        .update(|settings| settings.label = Some("Kitchen".into()))
        .expect("current interactive user can update the config");
    assert_private_acl(&config, &user_sid, true);
    let temp_probe = dir.join("config.json.tmp");
    fs::write(&temp_probe, b"").expect("create replacement temp file");
    assert_private_acl(&temp_probe, &user_sid, false);

    let damaged =
        std::env::temp_dir().join(format!("gourmely-acl-damaged-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&damaged).expect("create damaged fixture directory");
    let _damaged_sandbox = Sandbox(damaged.clone());
    let damaged_config = damaged.join("config.json");
    let invalid = b"{not valid JSON but may contain credentials";
    fs::write(&damaged_config, invalid).expect("write damaged config");
    std::env::set_var("GOURMELYPRINT_DATA_DIR", &damaged);
    let fallback = SettingsStore::load();
    assert!(
        fallback
            .security_issue()
            .unwrap_or_default()
            .contains("administrador"),
        "the operator needs an explicit elevated repair action"
    );
    assert!(fallback
        .update(|s| s.label = Some("Unsafe".into()))
        .is_err());
    assert_eq!(
        fs::read(damaged_config).expect("read damaged config"),
        invalid
    );
}
