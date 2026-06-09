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
//!   • Imprimir prueba                        (sends a test ticket to the default printer)
//!   • Salir                                  (quits the process)
//!
//! Kept deliberately small — every extra item is a support call. The
//! live status (connected / version / uptime) lives in the settings
//! window, not the tray, so the menu has no stale "iniciando…" label.
//! "Ver logs" and "Acerca de" were removed: logs aren't written in V1
//! and "Acerca de" linked to GitHub, which customers shouldn't see.

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, Runtime,
};

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
    let open_settings = MenuItem::with_id(&handle, "open", "Abrir configuración", true, None::<&str>)?;
    let test_print = MenuItem::with_id(&handle, "test", "Imprimir prueba", true, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(&handle)?;
    let quit = MenuItem::with_id(&handle, "quit", "Salir", true, None::<&str>)?;

    let menu = Menu::with_items(
        &handle,
        &[&header, &sep1, &open_settings, &test_print, &sep2, &quit],
    )?;

    TrayIconBuilder::with_id("main")
        .tooltip("GourmelyPrint Bridge")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "test" => spawn_test_print(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    } else {
        tracing::warn!("main window not found when trying to show");
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

