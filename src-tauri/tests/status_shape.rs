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

use print_bridge_lib::relay::{RelayState, RelayStatus};

/// Field names read by `renderRelay` in `src/main.ts`.
const READ_BY_THE_UI: &[&str] = &[
    "state",
    "paired",
    "connected",
    "serverUrl",
    "label",
    "roles",
    "lastError",
    "tokenRejected",
    "blockedByAnotherInstance",
    "ledgerRecovered",
    "jobsPrinted",
    "jobsFailed",
];

#[test]
fn the_settings_window_gets_the_field_names_it_reads() {
    let status = RelayStatus {
        state: RelayState::Standby,
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

/// Every state the relay can be in has to be a string `src/main.ts` knows how
/// to label.
///
/// The panel used to derive its wording from three booleans, so anything
/// without a boolean of its own fell through to "Reconectando…" — which is how
/// a bridge that could not open its print ledger came to show the sentence for
/// a dead router. Now it renders `state`, and a state the UI has no label for
/// would silently do the same thing again. This is the list, and it is checked
/// against the enum rather than against memory.
#[test]
fn every_state_has_a_label_in_the_settings_window() {
    // Copied from `RELAY_STATE_LABEL` in src/main.ts.
    const LABELLED_BY_THE_UI: &[&str] = &[
        "unpaired",
        "connecting",
        "connected",
        "rejected",
        "standby",
        "ledger-unavailable",
    ];
    for state in RelayState::ALL {
        let wire = serde_json::to_value(state)
            .expect("serialize")
            .as_str()
            .map(str::to_string)
            .expect("a string");
        assert!(
            LABELLED_BY_THE_UI.contains(&wire.as_str()),
            "`{wire}` reaches the settings window with no label; add it to \
             RELAY_STATE_LABEL in src/main.ts or the panel falls back to the \
             wording for a problem this is not"
        );
    }
    assert_eq!(
        LABELLED_BY_THE_UI.len(),
        RelayState::ALL.len(),
        "the UI labels a state the relay cannot be in"
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
