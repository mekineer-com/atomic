import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

// Execute the actual cleanup block without starting Vite, Docker, or a DB.
const source = readFileSync(new URL('./dev-server.js', import.meta.url), 'utf8');
const cleanup = source.slice(source.indexOf('// Clean shutdown on Ctrl+C'));

function fixture(platform) {
  const signals = [];
  const children = [1, 2].map((pid) => ({
    pid, exitCode: null, signalCode: null, killed: false,
    kill(signal) { signals.push([pid, signal]); this.killed = true; },
  }));
  const handlers = {};
  let dockerStops = 0;
  const context = {
    children, stopping: false, dockerStarted: true,
    process: { platform, on(name, handler) { handlers[name] = handler; } },
    root: '/fictional', resolve, console: { log() {} },
    execSync() { dockerStops++; },
    setTimeout() { throw Error('ordinary stop must not schedule escalation'); },
  };
  runInNewContext(cleanup, context);
  return { children, signals, handlers, context, dockerStops: () => dockerStops };
}

test('Windows console stop never kills children and waits for both before Docker cleanup', () => {
  const f = fixture('win32');
  f.handlers.SIGINT();
  f.handlers.SIGINT();
  f.handlers.SIGTERM();
  assert.deepEqual(f.signals, []);
  assert.equal(f.dockerStops(), 0);
  f.children[0].exitCode = 0;
  f.context.finishCleanup();
  assert.equal(f.dockerStops(), 0);
  f.children[1].exitCode = 0;
  f.context.finishCleanup();
  f.context.finishCleanup();
  assert.equal(f.dockerStops(), 1);
});

test('POSIX preserves one SIGTERM per live child and waits for exit', () => {
  const f = fixture('linux');
  f.children[0].exitCode = 0;
  f.handlers.SIGINT();
  f.handlers.SIGTERM();
  assert.deepEqual(f.signals, [[2, 'SIGTERM']]);
  assert.equal(f.dockerStops(), 0);
  f.children[1].signalCode = 'SIGTERM';
  f.context.finishCleanup();
  assert.equal(f.dockerStops(), 1);
});
