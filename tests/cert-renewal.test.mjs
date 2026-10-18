import assert from 'node:assert/strict';
import { createPrivateKey, createPublicKey, X509Certificate } from 'node:crypto';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), 'utf8');
const DAY_MS = 24 * 60 * 60 * 1000;
const HOST = 'localhost.gourmelyhub.busticco.com';
const MINIMUM_RUNWAY_DAYS = 30;

function scheduledAttempts(cron, expiresAt, windowDays) {
  const [minute, hour, monthDay, month, weekDay] = cron.split(' ');
  assert.equal(month, '*', 'the renewal schedule must run in every month');

  const attempts = [];
  for (let daysBeforeExpiry = windowDays; daysBeforeExpiry >= 0; daysBeforeExpiry--) {
    const at = new Date(expiresAt.getTime() - daysBeforeExpiry * DAY_MS);
    at.setUTCHours(Number(hour), Number(minute), 0, 0);
    if (at >= expiresAt) continue;
    const matchesMonthDay = monthDay === '*' || Number(monthDay) === at.getUTCDate();
    const matchesWeekDay = weekDay === '*' || Number(weekDay) === at.getUTCDay();
    if (matchesMonthDay && matchesWeekDay) attempts.push(at);
  }
  return attempts;
}

function assertEmbeddedCertificateReady(now) {
  const certificate = new X509Certificate(read('src-tauri/certs/fullchain.pem'));
  const key = createPrivateKey(read('src-tauri/certs/privkey.pem'));
  assert.equal(certificate.checkHost(HOST), HOST, 'embedded certificate SAN');
  assert.deepEqual(
    certificate.publicKey.export({ format: 'der', type: 'spki' }),
    createPublicKey(key).export({ format: 'der', type: 'spki' }),
    'embedded certificate and private key must match',
  );
  assert.ok(Date.parse(certificate.validFrom) <= now, 'embedded certificate is not yet valid');
  assert.ok(
    Date.parse(certificate.validTo) - now >= MINIMUM_RUNWAY_DAYS * DAY_MS,
    'embedded certificate has less than 30 days of publication runway',
  );
}

test('renewal has repeated scheduled attempts before embedded certificate expiry', () => {
  const workflow = read('.github/workflows/cert-renew.yml');
  const cron = workflow.match(/- cron: "([^"]+)"/)?.[1];
  const windowDays = Number(workflow.match(/RENEW_WINDOW_DAYS: "(\d+)"/)?.[1]);
  assert.ok(cron, 'renewal cron must be defined');
  assert.ok(Number.isInteger(windowDays), 'renewal window must be defined');
  const certificate = new X509Certificate(read('src-tauri/certs/fullchain.pem'));
  const attempts = scheduledAttempts(cron, new Date(certificate.validTo), windowDays);
  assert.ok(
    attempts.length >= 4,
    `only ${attempts.length} renewal attempt(s) before expiry: ${attempts.join(', ')}`,
  );
});

test('CI runs the embedded certificate publication gate', () => {
  const scripts = JSON.parse(read('package.json')).scripts;
  assert.equal(scripts['test:certificate'], 'node --test tests/cert-renewal.test.mjs');
  assert.match(read('.github/workflows/ci.yml'), /npm run test:certificate/);
});

test('the embedded pair is valid now but fails the gate near expiry', () => {
  const certificate = new X509Certificate(read('src-tauri/certs/fullchain.pem'));
  assertEmbeddedCertificateReady(Date.now());
  assert.throws(
    () => assertEmbeddedCertificateReady(Date.parse(certificate.validTo) - 29 * DAY_MS),
    /less than 30 days/,
  );
});
