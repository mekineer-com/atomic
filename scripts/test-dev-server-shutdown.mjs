import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';
import { EventEmitter } from 'node:events';

// Execute the actual cleanup block without starting Vite, Docker, or a DB.
const source = readFileSync(new URL('./dev-server.js', import.meta.url), 'utf8');
const cleanup = source.slice(source.indexOf('// Clean shutdown on Ctrl+C'));
const startProcess = source.slice(source.indexOf('function startProcess('), source.indexOf('// Start Postgres'));

function fixture(platform) {
  const signals = [];
  const children = [];
  const handlers = {};
  let dockerStops = 0;
  const context = {
    children, stopping: false, dockerStarted: true,
    process: { platform, env: {}, on(name, handler) { handlers[name] = handler; } },
    root: '/fictional', resolve, console: { log() {}, error() {} },
    spawn() {
      return Object.assign(new EventEmitter(), {
        pid: children.length + 1, exitCode: null, signalCode: null, killed: false,
        stdout: new EventEmitter(), stderr: new EventEmitter(),
        kill(signal) { signals.push([this.pid, signal]); this.killed = true; },
      });
    },
    execSync() { dockerStops++; },
    setTimeout() { throw Error('ordinary stop must not schedule escalation'); },
  };
  runInNewContext(startProcess + cleanup, context);
  context.startProcess('api', 'fictional-api', []);
  context.startProcess('web', 'fictional-web', []);
  return { children, signals, handlers, context, dockerStops: () => dockerStops };
}

test('Windows console stop never kills children and waits for both before Docker cleanup', () => {
  const f = fixture('win32');
  f.handlers.SIGINT('SIGINT');
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

for (const failure of ['exit', 'spawn']) {
  test(`Windows API ${failure} stops Vite without waiting for user Stop`, () => {
    const f = fixture('win32');
    const api = f.children[0];
    if (failure === 'exit') {
      api.exitCode = 7;
      api.emit('exit', 7);
    } else {
      api.pid = undefined;
      api.emit('error', new Error('Fictional spawn failure'));
    }
    assert.deepEqual(f.signals, [[2, 'SIGTERM']]);
    assert.equal(f.context.process.exitCode, failure === 'exit' ? 7 : 1);
    assert.equal(f.dockerStops(), 0);
    f.children[1].signalCode = 'SIGTERM';
    f.children[1].emit('close');
    assert.equal(f.dockerStops(), 1);
  });
}

test('Windows Vite crash never force-kills API work', () => {
  const f = fixture('win32');
  f.children[1].exitCode = 3;
  f.children[1].emit('exit', 3);
  assert.deepEqual(f.signals, []);
  assert.equal(f.dockerStops(), 0);
  f.handlers.SIGINT('SIGINT');
  assert.deepEqual(f.signals, []);
});

test('Windows live API error preserves work without hiding a later API exit', () => {
  const f = fixture('win32');
  f.children[0].emit('error', new Error('Fictional live-child error'));
  assert.deepEqual(f.signals, []);
  assert.equal(f.context.stopping, false);
  f.children[0].exitCode = 7;
  f.children[0].emit('exit', 7);
  assert.deepEqual(f.signals, [[2, 'SIGTERM']]);
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
