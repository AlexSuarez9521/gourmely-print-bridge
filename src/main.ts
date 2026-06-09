/**
 * GourmelyPrint Bridge — settings UI.
 *
 * Wires three small panels (Estado, Impresoras, Ajustes) to the Rust
 * commands exposed by `lib.rs`. Kept deliberately framework-free —
 * three panels don't earn a React dependency, and the bundle is
 * smaller when we ship a Windows installer.
 */
import { invoke } from '@tauri-apps/api/core';
import { check, type Update } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';

const APP_BOOT_AT = Date.now();
const TICK_MS = 5_000;

interface HealthSnapshot {
  /** Mirror of the Rust `HealthResponse`. */
  ok: boolean;
  version: string;
  uptime_seconds: number;
  printer_count: number;
}

// ─── Tab switching ───────────────────────────────────────────────────

function initTabs() {
  const tabs = document.querySelectorAll<HTMLButtonElement>('.tab');
  const panels = document.querySelectorAll<HTMLElement>('.panel');
  tabs.forEach((tab) => {
    tab.addEventListener('click', () => {
      const target = tab.dataset.tab;
      tabs.forEach((t) => t.classList.toggle('is-active', t === tab));
      panels.forEach((p) => p.classList.toggle('is-active', p.dataset.panel === target));
    });
  });
}

// ─── Estado tab ──────────────────────────────────────────────────────

async function refreshStatus() {
  const dot = document.getElementById('status-dot');
  const conn = document.getElementById('conn-status');
  const count = document.getElementById('printer-count');
  const version = document.getElementById('version');
  const versionValue = document.getElementById('version-value');
  const uptime = document.getElementById('uptime');

  try {
    const res = await fetch('https://localhost.gourmelyhub.busticco.com:8181/health', {
      // Same-origin would 404; we're talking to the in-process server.
      method: 'GET',
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const body = (await res.json()) as HealthSnapshot;
    if (conn) conn.textContent = 'Conectado';
    if (count) count.textContent = String(body.printer_count);
    if (version) version.textContent = `v${body.version}`;
    if (versionValue) versionValue.textContent = `v${body.version}`;
    if (uptime) uptime.textContent = formatUptime(body.uptime_seconds);
    if (dot) dot.classList.remove('is-error', 'is-warn');
  } catch (e) {
    if (conn) conn.textContent = 'Sin conexión';
    if (count) count.textContent = '—';
    if (uptime) uptime.textContent = '—';
    if (dot) {
      dot.classList.add('is-error');
      dot.classList.remove('is-warn');
    }
    console.warn('health probe failed', e);
  }
}

function formatUptime(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m ${seconds % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

// ─── Impresoras tab ──────────────────────────────────────────────────

async function refreshPrinterList() {
  const list = document.getElementById('printer-list');
  if (!list) return;
  try {
    const printers = await invoke<string[]>('list_printers');
    if (printers.length === 0) {
      list.innerHTML = '<li class="printer-empty">Sin impresoras instaladas</li>';
      return;
    }
    list.innerHTML = '';
    for (const name of printers) {
      const li = document.createElement('li');
      li.className = 'printer-item';
      const span = document.createElement('span');
      span.className = 'printer-name';
      span.textContent = name;
      const btn = document.createElement('button');
      btn.className = 'btn-primary';
      btn.textContent = 'Imprimir prueba';
      btn.addEventListener('click', () => testPrint(name, btn));
      li.appendChild(span);
      li.appendChild(btn);
      list.appendChild(li);
    }
  } catch (e) {
    list.innerHTML = `<li class="printer-empty">Error: ${String(e)}</li>`;
  }
}

async function testPrint(printer: string, btn: HTMLButtonElement) {
  const original = btn.textContent;
  btn.disabled = true;
  btn.textContent = 'Enviando…';
  try {
    await invoke('test_print', { printerName: printer });
    btn.textContent = '✓ Enviado';
    setTimeout(() => {
      btn.textContent = original;
      btn.disabled = false;
    }, 2000);
  } catch (e) {
    btn.textContent = '✗ Falló';
    btn.disabled = false;
    console.error('test print failed', e);
    setTimeout(() => {
      btn.textContent = original;
    }, 3000);
  }
}

// ─── Ajustes tab ─────────────────────────────────────────────────────

async function initSettings() {
  const toggle = document.getElementById('autostart-toggle') as HTMLInputElement | null;
  if (toggle) {
    try {
      toggle.checked = await invoke<boolean>('is_autostart_enabled');
    } catch (e) {
      console.warn('is_autostart_enabled failed', e);
    }
    toggle.addEventListener('change', async () => {
      const wanted = toggle.checked;
      try {
        await invoke('set_autostart', { enabled: wanted });
      } catch (e) {
        console.error('set_autostart failed', e);
        toggle.checked = !wanted; // revert
      }
    });
  }

}

// ─── Auto-update ─────────────────────────────────────────────────────

/**
 * Check the signed `latest.json` manifest for a newer version. When one
 * is available, show a banner with one-click "Instalar" + "Después".
 * The check is best-effort — a network failure (offline cashier PC) is
 * silently ignored so it never blocks the bridge from running.
 */
async function checkForUpdate() {
  let update: Update | null = null;
  try {
    update = await check();
  } catch (e) {
    console.warn('update check failed (offline?)', e);
    return;
  }
  if (!update?.available) return;
  showUpdateBanner(update);
}

function showUpdateBanner(update: Update) {
  // Build the banner once, lazily, so the happy path (no update) adds
  // zero DOM. Inserted at the top of the app shell.
  let banner = document.getElementById('update-banner');
  if (!banner) {
    banner = document.createElement('div');
    banner.id = 'update-banner';
    banner.className = 'update-banner';
    document.body.prepend(banner);
  }
  banner.innerHTML = `
    <span class="update-text">Nueva versión ${update.version} disponible.</span>
    <button class="btn-primary" id="update-install">Instalar y reiniciar</button>
    <button class="btn-ghost" id="update-later">Después</button>
  `;

  document.getElementById('update-later')?.addEventListener('click', () => {
    banner?.remove();
  });

  document.getElementById('update-install')?.addEventListener('click', async () => {
    const installBtn = document.getElementById('update-install') as HTMLButtonElement | null;
    const laterBtn = document.getElementById('update-later') as HTMLButtonElement | null;
    if (installBtn) {
      installBtn.disabled = true;
      installBtn.textContent = 'Descargando…';
    }
    if (laterBtn) laterBtn.disabled = true;
    try {
      // downloadAndInstall streams progress events; we keep the UI
      // simple and just flip the label. On Windows the MSI installer
      // runs and the app exits, so relaunch() resumes it.
      await update.downloadAndInstall();
      if (installBtn) installBtn.textContent = 'Reiniciando…';
      await relaunch();
    } catch (e) {
      console.error('update install failed', e);
      if (installBtn) {
        installBtn.disabled = false;
        installBtn.textContent = 'Reintentar';
      }
      if (laterBtn) laterBtn.disabled = false;
    }
  });
}

// ─── Boot ────────────────────────────────────────────────────────────

window.addEventListener('DOMContentLoaded', () => {
  initTabs();
  initSettings();
  refreshStatus();
  refreshPrinterList();
  setInterval(refreshStatus, TICK_MS);
  // Best-effort update check on boot. Never blocks the UI.
  void checkForUpdate();
  console.info(`GourmelyPrint Bridge UI booted at ${new Date(APP_BOOT_AT).toISOString()}`);
});
