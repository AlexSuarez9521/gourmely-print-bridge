//! The exact JSON `relay_status` hands the settings window.
//!
//! # Why this is a test and not a comment
//!
//! `RelayStatus` is a Rust struct and `src/main.ts` declares its own interface
//! for the same object. Nothing connects the two but serde's `camelCase` rename
//! and a developer's memory, and a mismatch does not fail a build on either
//! side: the field simply arrives as `undefined`, every branch reading it takes
//! the falsy path, and the panel keeps rendering a perfectly plausible screen.
//!
//! For `blockedByAnotherInstance` that plausible screen is the bug it was added
//! to remove — a bridge that will not print, showing "Reconectando…". So the
//! wire name is pinned here, next to a note of who reads it.

use print_bridge_lib::relay::RelayStatus;

/// Field names read by `renderRelay` in `src/main.ts`.
const READ_BY_THE_UI: &[&str] = &[
    "paired",
    "connected",
    "serverUrl",
    "label",
    "roles",
    "lastError",
    "tokenRejected",
    "blockedByAnotherInstance",
    "jobsPrinted",
    "jobsFailed",
];

#[test]
fn the_settings_window_gets_the_field_names_it_reads() {
    let status = RelayStatus {
        paired: true,
        blocked_by_another_instance: true,
        ..Default::default()
    };
    let json = serde_json::to_value(&status).expect("serialize");
    let object = json.as_object().expect("an object");
    println!("{}", serde_json::to_string_pretty(&json).expect("pretty"));

    for field in READ_BY_THE_UI {
        assert!(
            object.contains_key(*field),
            "src/main.ts reads `{field}`, which is not on the wire; it will be \
             undefined there and every check on it silently takes the false branch"
        );
    }

    // The one whose absence is invisible AND dangerous.
    assert_eq!(
        object.get("blockedByAnotherInstance"),
        Some(&serde_json::Value::Bool(true)),
        "the standing-down flag must reach the UI, or the panel shows \
         'Reconectando…' for a bridge that is not going to print"
    );
}

/// The token is the station's credential. It has never been part of this
/// struct and must not become one — the settings window is a webview.
#[test]
fn the_station_token_is_not_in_there() {
    let json = serde_json::to_string(&RelayStatus {
        paired: true,
        ..Default::default()
    })
    .expect("serialize");
    for leak in ["token\"", "stationToken", "secret"] {
        assert!(
            !json.contains(leak),
            "`{leak}` reached the webview:\n{json}"
        );
    }
}
