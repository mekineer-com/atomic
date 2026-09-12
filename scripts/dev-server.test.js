import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { afterEach, describe, it, mock } from 'node:test';
import assert from 'node:assert/strict';

const originalSigint = process.listeners('SIGINT');
const originalSigterm = process.listeners('SIGTERM');
const originalExitCode = process.exitCode;

afterEach(() => {
  for (const listener of process.listeners('SIGINT')) {
    if (!originalSigint.includes(listener)) process.removeListener('SIGINT', listener);
  }
  for (const listener of process.listeners('SIGTERM')) {
    if (!originalSigterm.includes(listener)) process.removeListener('SIGTERM', listener);
  }
  process.exitCode = originalExitCode;
  mock.restoreAll();
});

describe('dev server child supervision', () => {
  it('cleans up siblings once and records failure on a spawn error', async () => {
    const children = [];
    mock.module('node:child_process', {
      exports: {
        execSync() {},
        spawn() {
          const child = new EventEmitter();
          child.stdout = new PassThrough();
          child.stderr = new PassThrough();
          child.killed = false;
          child.killCount = 0;
          child.kill = () => {
            child.killed = true;
            child.killCount += 1;
          };
          children.push(child);
          return child;
        },
        spawnSync() { return { stdout: '', stderr: '' }; },
      },
    });

    await import(`./dev-server.js?test=${Date.now()}`);
    assert.equal(children.length, 2);

    children[1].emit('error', new Error('cannot execute'));
    children[1].emit('exit', 1);
    children[1].emit('error', new Error('duplicate notification'));

    assert.equal(process.exitCode, 1);
    assert.equal(children[0].killCount, 1);
    assert.equal(children[1].killCount, 1);
  });
});
