import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const config = JSON.parse(readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));

test('packaged WebView CSP blocks inline script and permits only required connections', () => {
  const csp = config.app.security.csp;
  assert.equal(typeof csp, 'string');
  assert.match(csp, /(?:^|;)\s*default-src 'self'/);
  assert.match(csp, /(?:^|;)\s*script-src 'self'/);
  assert.match(csp, /(?:^|;)\s*connect-src [^;]*ipc: [^;]*http:\/\/ipc\.localhost [^;]*https:\/\/localhost\.gourmelyhub\.busticco\.com:8181/);
  assert.doesNotMatch(csp, /'unsafe-inline'|'unsafe-eval'|\bhttps:\s|\bhttp:\s/);
});
