#!/usr/bin/env node

/**
 * Development helper: starts the atomic-server API and the Vite web frontend concurrently.
 *
 * Usage:
 *   npm run dev:server                           # SQLite mode (default), API on :8080, Vite on :1420
 *   npm run dev:server -- --postgres             # Postgres mode, auto-starts Docker pgvector
 *   npm run dev:server -- --port 9000            # Custom port
 *   npm run dev:server -- --data-dir /path/to/data
 *   npm run dev:server -- --postgres --database-url postgres://user:pass@host:5432/db
 *
 * Options:
 *   --postgres           Use Postgres backend (starts pgvector via docker-compose if needed)
 *   --database-url URL   Postgres connection string (implies --postgres)
 *   --no-docker          Don't auto-start Docker for Postgres (assumes external DB)
 *   --port PORT          Server port (default: 8080)
 *   --bind ADDR          Bind address (default: 0.0.0.0)
 *   --data-dir PATH      Data directory for registry.db
 *   --setup-token TOKEN  Override the default dev setup-token bypass
 *
 * All other flags are forwarded to atomic-server. By default this helper passes
 * --dangerously-skip-setup-token for local development.
 * Both processes are killed together on Ctrl+C.
 */

import { spawn, execSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = resolve(__dirname, '..');

// Parse our own flags, forward the rest
const rawArgs = process.argv.slice(2);
let usePostgres = false;
let noDocker = false;
let databaseUrl = null;
const forwardArgs = [];

for (let i = 0; i < rawArgs.length; i++) {
  const arg = rawArgs[i];
  if (arg === '--postgres') {
    usePostgres = true;
  } else if (arg === '--no-docker') {
    noDocker = true;
  } else if (arg === '--database-url') {
    databaseUrl = rawArgs[++i];
    usePostgres = true;
  } else {
    forwardArgs.push(arg);
  }
}

const DEFAULT_PG_URL = 'postgres://atomic:atomic_dev@localhost:5434/atomic_dev';

if (usePostgres && !databaseUrl) {
  databaseUrl = DEFAULT_PG_URL;
}

// Build server args
const hasServeSubcommand = forwardArgs.includes('serve');
const serverArgs = hasServeSubcommand
  ? forwardArgs
  : [...forwardArgs, 'serve', '--bind', '0.0.0.0'];

const hasSetupBypassEnv = Object.prototype.hasOwnProperty.call(
  process.env,
  'ATOMIC_DANGEROUSLY_SKIP_SETUP_TOKEN'
);

if (
  !hasSetupBypassEnv &&
  !serverArgs.includes('--setup-token') &&
  !serverArgs.includes('--dangerously-skip-setup-token')
) {
  serverArgs.push('--dangerously-skip-setup-token');
}

if (usePostgres) {
  if (!serverArgs.includes('--storage')) {
    serverArgs.push('--storage', 'postgres');
  }
  if (!serverArgs.includes('--database-url')) {
    serverArgs.push('--database-url', databaseUrl);
  }
}

const children = [];
let dockerStarted = false;

function startProcess(name, command, args, opts = {}) {
  const proc = spawn(command, args, {
    cwd: root,
    stdio: 'pipe',
    env: { ...process.env, ...opts.env },
  });

  const prefix = name.padEnd(8);
  const color = opts.color || '\x1b[36m';

  proc.stdout.on('data', (data) => {
    for (const line of data.toString().split('\n').filter(Boolean)) {
      console.log(`${color}[${prefix}]\x1b[0m ${line}`);
    }
  });

  proc.stderr.on('data', (data) => {
    for (const line of data.toString().split('\n').filter(Boolean)) {
      console.log(`\x1b[33m[${prefix}]\x1b[0m ${line}`);
    }
  });

  proc.on('exit', (code) => {
    console.log(`\x1b[90m[${prefix}] exited (code ${code})\x1b[0m`);
  });

  children.push(proc);
  return proc;
}

// Start Postgres via Docker if needed
if (usePostgres && !noDocker) {
  console.log('\x1b[35m[postgres]\x1b[0m Starting pgvector via Docker...');

  // Write a dev-specific docker-compose that uses port 5434 to avoid conflicting with test (5433) or prod (5432)
  const composeFile = resolve(root, 'docker-compose.dev.yml');
  const fs = await import('node:fs');
  fs.writeFileSync(composeFile, `services:
  postgres:
    image: pgvector/pgvector:pg16
    environment:
      POSTGRES_DB: atomic_dev
      POSTGRES_USER: atomic
      POSTGRES_PASSWORD: atomic_dev
    ports:
      - "5434:5432"
    volumes:
      - atomic-dev-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U atomic"]
      interval: 2s
      timeout: 5s
      retries: 5

volumes:
  atomic-dev-data:
`);

  try {
    execSync(`docker compose -f ${composeFile} up -d --wait`, {
      cwd: root,
      stdio: 'inherit',
    });
    dockerStarted = true;
    console.log('\x1b[35m[postgres]\x1b[0m pgvector ready on port 5434');
  } catch (e) {
    console.error('\x1b[31m[postgres]\x1b[0m Failed to start Docker. Is Docker running?');
    console.error('\x1b[31m[postgres]\x1b[0m Use --no-docker with --database-url to connect to an external Postgres.');
    process.exit(1);
  }
}

// Print mode
const mode = usePostgres ? `Postgres (${databaseUrl})` : 'SQLite';
console.log(`\n\x1b[1m  Storage: ${mode}\x1b[0m\n`);

// Start atomic-server (postgres feature is always compiled in via atomic-server's Cargo.toml)
const serverBin = process.env.ATOMIC_SERVER_BIN;
if (serverBin && !existsSync(serverBin)) {
  console.error(`Atomic server binary not found: ${serverBin}`);
  process.exit(1);
}
const serverCommand = serverBin || 'cargo';
const serverCommandArgs = serverBin
  ? serverArgs
  : ['run', '-p', 'atomic-server', '--', ...serverArgs];

startProcess('api', serverCommand, serverCommandArgs, { color: '\x1b[36m' });

// Start Vite dev server in web mode
startProcess('web', 'npx', ['vite', '--host'], {
  env: { VITE_BUILD_TARGET: 'web' },
  color: '\x1b[32m',
});

// Clean shutdown on Ctrl+C
function cleanup() {
  for (const child of children) {
    if (!child.killed) {
      child.kill('SIGTERM');
    }
  }

  if (dockerStarted) {
    console.log('\x1b[35m[postgres]\x1b[0m Stopping Docker containers...');
    try {
      execSync(`docker compose -f ${resolve(root, 'docker-compose.dev.yml')} stop`, {
        cwd: root,
        stdio: 'inherit',
      });
    } catch {}
  }
}

process.on('SIGINT', cleanup);
process.on('SIGTERM', cleanup);
