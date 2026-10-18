//! The repair subcommand must exit before Tauri or the relay can start.

use std::process::Command;

#[test]
fn incomplete_repair_command_exits_without_starting_the_bridge() {
    let output = Command::new(env!("CARGO_BIN_EXE_print-bridge"))
        .arg("repair-acl")
        .output()
        .expect("launch bridge executable");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: print-bridge repair-acl"),
        "missing repair usage: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
