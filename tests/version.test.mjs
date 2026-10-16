import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

function read(relativePath) {
  return readFileSync(new URL(`../${relativePath}`, import.meta.url), 'utf8');
}

function cargoLockVersion(content) {
  return content.match(/^name = "print-bridge"\r?\nversion = "([^"]+)"/m)?.[1];
}

test('Cargo.lock version parsing tolerates Windows line endings', () => {
  assert.equal(cargoLockVersion(read('src-tauri/Cargo.lock').replace(/\r?\n/g, '\r\n')), '0.1.6');
});

test('all release metadata declares bridge v0.1.6', () => {
  const tauri = JSON.parse(read('src-tauri/tauri.conf.json'));
  const cargo = read('src-tauri/Cargo.toml');
  const cargoLock = read('src-tauri/Cargo.lock');
  const npm = JSON.parse(read('package.json'));
  const npmLock = JSON.parse(read('package-lock.json'));

  assert.deepEqual(
    {
      tauri: tauri.version,
      cargo: cargo.match(/^\[package\][\s\S]*?^version = "([^"]+)"/m)?.[1],
      cargoLock: cargoLockVersion(cargoLock),
      npm: npm.version,
      npmLock: npmLock.version,
      npmLockRoot: npmLock.packages[''].version,
    },
    Object.fromEntries(
      ['tauri', 'cargo', 'cargoLock', 'npm', 'npmLock', 'npmLockRoot'].map((name) => [name, '0.1.6']),
    ),
  );
});
