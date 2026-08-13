//! System tray icon + menu.
//!
//! The tray is the only UI surface the cashier sees most days — the
//! settings window stays hidden until they click "Open settings" from
//! the menu. We keep the menu small on purpose: every extra item is a
//! support call.
//!
//! Menu:
//!   • GourmelyPrint Bridge                   (header, disabled — branding)
//!   • Abrir configuración                    (opens main window)
//!   • Vincular estación…                     (opens the pairing panel)
//!   • Imprimir prueba                        (sends a test ticket to the default printer)
//!   • Salir                                  (quits the process)
//!
//! Kept deliberately small — every extra item is a support call. The
//! live status (connected / version / uptime) lives in the settings
//! window, not the tray, so the menu has no stale "iniciando…" label.
//! "Ver logs" and "Acerca de" were removed: logs aren't written in V1
//! and "Acerca de" linked to GitHub, which customers shouldn't see.
//!
//! "Vincular estación…" is the one addition, and it earns the slot: pairing is
//! the single thing someone has to do on a brand-new till, and the person doing
//! it is a cashier, not an engineer. It must be reachable from the icon by the
//! clock without knowing that a settings window exists — and it must never
//! involve editing a file by hand.

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, Runtime,
};

use crate::relay::RelayStatus;

/// Event the settings UI listens for to jump straight to the pairing panel.
pub const OPEN_PAIRING_EVENT: &str = "open-pairing";

/// Id of the tray icon, needed to reach it again after `install`.
const TRAY_ID: &str = "main";

/// Installs the tray icon on the running Tauri app. Call once at
/// startup from `lib.rs` inside `setup`.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let handle = app.clone();

    // Menu items — created up front so we can attach them to the menu
    // and to the click handler in one builder pass.
    let header = MenuItem::with_id(
        &handle,
        "header",
        "GourmelyPrint Bridge",
        false, // disabled — branding label, not a clickable item
        None::<&str>,
    )?;
    let sep1 = PredefinedMenuItem::separator(&handle)?;
    let open_settings =
        MenuItem::with_id(&handle, "open", "Abrir configuración", true, None::<&str>)?;
    let pair = MenuItem::with_id(&handle, "pair", "Vincular estación…", true, None::<&str>)?;
    let test_print = MenuItem::with_id(&handle, "test", "Imprimir prueba", true, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(&handle)?;
    let quit = MenuItem::with_id(&handle, "quit", "Salir", true, None::<&str>)?;

    let menu = Menu::with_items(
        &handle,
        &[
            &header,
            &sep1,
            &open_settings,
            &pair,
            &test_print,
            &sep2,
            &quit,
        ],
    )?;

    TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("GourmelyPrint Bridge")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "pair" => open_pairing(app),
            "test" => spawn_test_print(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/// The tooltip that goes under the mouse when someone hovers the tray icon.
///
/// # Why this is worth its lines
///
/// The tray icon is the only part of the bridge most tills ever show, and it
/// looks identical whether the relay is printing or has stood down because a
/// second copy holds the print ledger. That second state is the dangerous one:
/// a live process, a normal-looking icon, and not one ticket for the rest of
/// the day. Naming it on hover is the cheapest place to break that symmetry —
/// it costs no window, no click and no notification.
///
/// A pure function of the status so it can be tested without a desktop.
pub fn tooltip_for(status: &RelayStatus) -> String {
    let line = if status.blocked_by_another_instance {
        "EN ESPERA — ya hay otro GourmelyPrint abierto; esta copia no imprime".to_string()
    } else if !status.paired {
        "Sin vincular — abre Ajustes → Estación".to_string()
    } else if status.token_rejected {
        "Estación rechazada — vuelve a vincularla".to_string()
    } else if status.connected {
        match status.label.as_deref() {
            Some(label) => format!("Conectado — {label}"),
            None => "Conectado".to_string(),
        }
    } else {
        "Reconectando…".to_string()
    };
    format!("GourmelyPrint Bridge\n{line}")
}

/// Pushes a new tooltip onto the installed tray icon.
///
/// Best effort: a tooltip that fails to update is not worth a log line at
/// error, and there is nothing the bridge could do about it anyway.
pub fn set_tooltip<R: Runtime>(app: &AppHandle<R>, text: &str) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        tracing::debug!("no tray icon to update the tooltip on");
        return;
    };
    if let Err(e) = tray.set_tooltip(Some(text)) {
        tracing::debug!(err = %e, "could not update the tray tooltip");
    }
}

/// Brings the settings window to the front, whatever state it is in.
///
/// `pub(crate)` because the single-instance handler in [`crate::run`] needs
/// exactly this: when the cashier double-clicks the shortcut on a bridge that
/// is already running, the second process dies and *this* is what they see, so
/// the double-click still looks like it did something.
///
/// `unminimize` first: with close-to-tray the window is usually hidden, but it
/// can also be minimised, and `show` on a minimised window leaves it on the
/// taskbar — which reads as "nothing happened" and earns a second double-click.
pub(crate) fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    } else {
        tracing::warn!("main window not found when trying to show");
    }
}

/// Opens the window and tells the UI to land on the pairing panel.
///
/// The event is emitted after `show`, and the UI also checks a flag on boot:
/// the window may still be building its DOM when this fires, and a pairing
/// screen that only appears sometimes is worse than one more click.
fn open_pairing<R: Runtime>(app: &AppHandle<R>) {
    show_main_window(app);
    if let Err(e) = app.emit(OPEN_PAIRING_EVENT, ()) {
        tracing::warn!("could not ask the UI to open the pairing panel: {e}");
    }
}

fn spawn_test_print<R: Runtime>(app: &AppHandle<R>) {
    // Fire-and-forget on a background task — the menu callback must
    // return quickly so the tray menu doesn't appear frozen.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let printers = match crate::printer::list() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("tray test print: list failed: {e}");
                return;
            }
        };
        let Some(first) = printers.first() else {
            tracing::warn!("tray test print: no printers installed");
            return;
        };
        // Re-uses the same ESC/POS template as the WS `test` op so the
        // two flows print byte-identical pages.
        let bytes = crate::printer::test_ticket_bytes();
        match crate::printer::print_raw(first, &bytes) {
            Ok(job) => tracing::info!("tray test print sent to {} (job={})", first, job),
            Err(e) => tracing::error!("tray test print failed: {e}"),
        }
        // Touch app so the closure captures it (avoids compiler warning
        // about unused move).
        let _ = app.config();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing_down() -> RelayStatus {
        RelayStatus {
            paired: true,
            connected: false,
            blocked_by_another_instance: true,
            last_error: Some("GourmelyPrint Bridge ya está abierto en este equipo".into()),
            ..Default::default()
        }
    }

    /// The tooltip has to distinguish the two states that look identical from
    /// outside: a bridge that is printing, and one that has stood down for
    /// another copy and never will.
    #[test]
    fn a_bridge_that_stood_down_does_not_look_healthy_on_hover() {
        let tip = tooltip_for(&standing_down());
        assert!(tip.contains("EN ESPERA"), "{tip}");
        assert!(tip.contains("no imprime"), "{tip}");

        let working = RelayStatus {
            paired: true,
            connected: true,
            label: Some("Caja Principal".into()),
            ..Default::default()
        };
        assert!(tooltip_for(&working).contains("Conectado — Caja Principal"));
        assert_ne!(tooltip_for(&standing_down()), tooltip_for(&working));
    }

    /// Standing down is not the same problem as a revoked token or a dead
    /// network, and each sends support somewhere different.
    #[test]
    fn every_state_gets_its_own_sentence() {
        let unpaired = RelayStatus::default();
        let rejected = RelayStatus {
            paired: true,
            token_rejected: true,
            ..Default::default()
        };
        let offline = RelayStatus {
            paired: true,
            ..Default::default()
        };
        let tips = [
            tooltip_for(&unpaired),
            tooltip_for(&rejected),
            tooltip_for(&offline),
            tooltip_for(&standing_down()),
        ];
        for (i, a) in tips.iter().enumerate() {
            for b in tips.iter().skip(i + 1) {
                assert_ne!(a, b, "two states share a tooltip");
            }
        }
        assert!(tips[0].contains("Sin vincular"));
        assert!(tips[1].contains("rechazada"));
        assert!(tips[2].contains("Reconectando"));
    }

    /// A bridge that lost the ledger before it was ever paired still has to
    /// explain itself — "Sin vincular" would send someone off to pair a copy
    /// that is not going to print regardless.
    #[test]
    fn standing_down_outranks_not_being_paired() {
        let never_paired = RelayStatus {
            paired: false,
            blocked_by_another_instance: true,
            ..Default::default()
        };
        assert!(tooltip_for(&never_paired).contains("EN ESPERA"));
    }
}
