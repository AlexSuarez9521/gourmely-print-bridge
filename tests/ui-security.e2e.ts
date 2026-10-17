import { readFileSync } from 'node:fs';
import { test, type Browser } from '@e2e-dev/web';
import { expect, type App } from 'e2e';

const injectedHtml = '<img src=x onerror="window.__bridgeXss=1">';
const appHtml = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const csp = JSON.parse(
  readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'),
).app.security.csp as string;

async function openWithMockedTauri(
  browser: Browser,
  app: App,
  scenario: 'printer-error' | 'update-version' | 'ledger-unavailable',
) {
  await browser.route('http://127.0.0.1:1420/', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'text/html',
      headers: { 'Content-Security-Policy': csp },
      body: appHtml,
    });
  });
  await browser.addInitScript(
    ({ scenario, injectedHtml }) => {
      const bridge = window as typeof window & {
        __bridgeXss?: number;
        __TAURI_INTERNALS__?: Record<string, unknown>;
        __TAURI_EVENT_PLUGIN_INTERNALS__?: Record<string, unknown>;
      };
      bridge.__bridgeXss = 0;
      bridge.__TAURI_INTERNALS__ = {
        invoke: async (command: string) => {
          if (command === 'list_printers') {
            if (scenario === 'printer-error') throw injectedHtml;
            return ['Thermal printer'];
          }
          if (command === 'plugin:updater|check') {
            return scenario === 'update-version'
              ? { rid: 1, currentVersion: '0.1.5', version: injectedHtml }
              : null;
          }
          if (command === 'plugin:event|listen') return 1;
          if (command === 'is_autostart_enabled') return false;
          if (command === 'local_secret') return null;
          if (command === 'relay_status') throw new Error('No relay in UI test');
          throw new Error(`Unexpected Tauri command: ${command}`);
        },
        transformCallback: () => 1,
        unregisterCallback: () => {},
      };
      bridge.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
      const nativeFetch = window.fetch.bind(window);
      window.fetch = (input, init) =>
        String(input).endsWith('/health')
          ? Promise.resolve(
              new Response(JSON.stringify({
                ok: true, version: '0.1.5', printer_count: 1,
                ...(scenario === 'ledger-unavailable' ? {
                  local_print_intents: {
                    ok: false, needs_attention: true,
                    detail: 'Impresión segura pausada: ejecute la reparación como administrador.',
                  },
                } : {}),
              }), {
                status: 200,
                headers: { 'Content-Type': 'application/json' },
              }),
            )
          : nativeFetch(input, init);
    },
    { scenario, injectedHtml },
  );
  await app.open();
}

test('printer error displays HTML literally without running it', async ({ app, browser, screen }) => {
  await openWithMockedTauri(browser, app, 'printer-error');
  await screen.getByRole('tab', 'Impresoras').tap();
  await expect(browser.locator('#printer-list')).toContainText(injectedHtml);
  await expect(browser.locator('#printer-list img')).toHaveCount(0);
  expect(await browser.evaluate(() => (window as typeof window & { __bridgeXss?: number }).__bridgeXss)).toBe(0);
});

test('update version displays HTML literally and dismissal still works', async ({ app, browser, screen }) => {
  await openWithMockedTauri(browser, app, 'update-version');
  await expect(browser.locator('#update-banner .update-text')).toContainText(injectedHtml);
  await expect(browser.locator('#update-banner img')).toHaveCount(0);
  expect(await browser.evaluate(() => (window as typeof window & { __bridgeXss?: number }).__bridgeXss)).toBe(0);
  await screen.getByRole('button', 'Después').tap();
  await expect(browser.locator('#update-banner')).toHaveCount(0);
});

test('configured CSP blocks an inline script in the browser', async ({ app, browser }) => {
  await browser.route('**/csp-probe', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'text/html',
      headers: { 'Content-Security-Policy': csp },
      body: '<div id="probe">Loaded</div><script>window.__bridgeXss=1</script>',
    });
  });
  await app.open('/csp-probe');
  await expect(browser.locator('#probe')).toHaveText('Loaded');
  expect(await browser.evaluate(() => (window as typeof window & { __bridgeXss?: number }).__bridgeXss)).toBeUndefined();
});

test('unavailable local intent ledger shows an actionable warning', async ({ app, browser }) => {
  await openWithMockedTauri(browser, app, 'ledger-unavailable');
  await expect(browser.locator('#conn-status')).toContainText('ejecute la reparación como administrador');
  expect(await browser.evaluate(() => document.getElementById('status-dot')?.classList.contains('is-warn'))).toBe(true);
});
