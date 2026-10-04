#!/usr/bin/env node
/**
 * One command to bring up a development stack:
 *
 *   node scripts/dev.mjs [--runtime-only|--no-runtime|--no-server|--no-web]
 *
 *   1. agentos-server    (Rust runtime: HTTP/WS gateway + gRPC)
 *   2. @agentos/server   (Node control server: proxy, WS fan-out, orchestration)
 *   3. @agentos/web      (Vite dev server)
 *
 * Running several stacks at once (one per checkout / working directory) is a supported mode, so
 * ports are resolved rather than assumed:
 *
 *   - RUNTIME_HTTP_PORT  RUNTIME_GRPC_PORT  CONTROL_PORT  WEB_PORT  pin a port explicitly;
 *   - a pinned port that is busy is a hard error (you asked for that port);
 *   - a default port that is busy is shifted to the next free one and the shift is announced,
 *     then propagated to whichever child needs to know (vite proxy target, control server URL).
 *
 * Children are spawned as node/cargo directly - no shell wrapper - so Ctrl-C reaches the real
 * process and arguments are never re-parsed by cmd.exe.
 */

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = new Set(process.argv.slice(2));
const wantRuntime = !args.has('--no-runtime');
const wantServer = !args.has('--no-server');
const wantWeb = !args.has('--no-web');
const runtimeOnly = args.has('--runtime-only');
const isWindows = process.platform === 'win32';

// --- ports ------------------------------------------------------------------------------------

function readPort(envName, fallback) {
  const raw = process.env[envName];
  if (raw === undefined || raw.trim() === '') return { value: fallback, pinned: false, envName };
  const parsed = Number(raw);
  if (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535) {
    console.error('[dev] ' + envName + ' must be a port number, got: ' + raw);
    process.exit(1);
  }
  return { value: parsed, pinned: true, envName };
}

/** Can we bind this port on a given host? */
function bindable(port, host) {
  return new Promise((resolve) => {
    const probe = net.createServer();
    probe.unref();
    probe.once('error', () => resolve(false));
    probe.once('listening', () => probe.close(() => resolve(true)));
    if (host === undefined) probe.listen(port);
    else probe.listen(port, host);
  });
}

/**
 * A port counts as free only if every address one of our children might bind accepts us.
 *
 * Windows treats these as independent: a process on 127.0.0.1:port does not block an
 * all-interfaces (::) bind, and one on [::1]:port does not block :: either. The runtime binds
 * 127.0.0.1, the control server binds everything and Vite binds localhost (which can resolve to
 * ::1 only). Probing a single family reported busy ports as free - the reason a second stack kept
 * colliding on 8788 and, later, on 5173.
 */
const PROBE_HOSTS = ['127.0.0.1', '::1', undefined]; // undefined = all interfaces

async function portFree(port) {
  for (const host of PROBE_HOSTS) {
    if (!(await bindable(port, host))) return false;
  }
  return true;
}

/**
 * Reserve ports one at a time. Probing is not enough on its own: nothing is bound yet, so two
 * roles could otherwise pick the same free port (that is exactly how the runtime once ended up
 * with its gateway and its gRPC endpoint on one address).
 */
const reserved = new Set();

async function resolvePort(label, requested) {
  if (!reserved.has(requested.value) && (await portFree(requested.value))) {
    reserved.add(requested.value);
    return requested;
  }
  if (requested.pinned) {
    console.error(
      '[dev] ' + label + ' port ' + requested.value + ' is already in use (' + requested.envName + ' is set).',
    );
    console.error('[dev] Either stop the other stack or pick another port, for example:');
    console.error('[dev]   ' + requested.envName + '=' + (requested.value + 2) + ' node scripts/dev.mjs');
    process.exit(1);
  }
  for (let candidate = requested.value + 1; candidate < requested.value + 200; candidate += 1) {
    if (!reserved.has(candidate) && (await portFree(candidate))) {
      reserved.add(candidate);
      console.log(
        '[dev] ' + label + ' port ' + requested.value + ' is busy, using ' + candidate + ' instead',
      );
      return { value: candidate, pinned: false, envName: requested.envName, shiftedFrom: requested.value };
    }
  }
  console.error('[dev] no free port found for ' + label + ' near ' + requested.value);
  process.exit(1);
}

const runtimeHttp = await resolvePort('runtime http', readPort('RUNTIME_HTTP_PORT', 8788));
const runtimeGrpc = await resolvePort('runtime grpc', readPort('RUNTIME_GRPC_PORT', 8789));
const controlPort = await resolvePort('control', readPort('CONTROL_PORT', 8790));
const webPort = await resolvePort('web', readPort('WEB_PORT', 5173));

const runtimeUrl = 'http://127.0.0.1:' + runtimeHttp.value;

// Two checkouts of this repository share a directory name, so a name made of the directory alone
// would leave two identical rows in the console's node list. The port is what actually tells the
// stacks apart on one machine, so it goes into the label.
const nodeName = process.env.AGENTOS_NODE_NAME ?? path.basename(repoRoot) + '-' + runtimeHttp.value;

// --- children ---------------------------------------------------------------------------------

/** Current child per role, so a restart replaces rather than accumulates. */
const children = new Map();
/** Restart timestamps per role, to stop a crash loop instead of feeding it. */
const restarts = new Map();
let stopping = false;

/**
 * Exit codes a Node process dies with on Windows when it does not die of its own accord. A bare
 * number tells nobody anything: 3221226505 in a terminal looks like noise, while "__fastfail" is
 * actionable.
 */
const EXIT_MEANINGS = new Map([
  [0xc0000005, 'access violation'],
  [0xc000013a, 'terminated by Ctrl+C'],
  [0xc0000142, 'DLL initialisation failed'],
  [0xc0000374, 'heap corruption'],
  [0xc0000409, '__fastfail: V8 fatal error, native stack overflow or out of memory'],
  [0xffffffff, 'killed by a job object or the task manager'],
]);

function describeExit(code, signal) {
  if (signal !== null && signal !== undefined) return 'killed by ' + signal;
  if (code === null) return 'killed';
  if (code === 0) return 'clean exit';
  const unsigned = code >>> 0;
  const meaning = EXIT_MEANINGS.get(unsigned);
  const hex = '0x' + unsigned.toString(16);
  return meaning === undefined ? 'code ' + code : 'code ' + code + ' (' + hex + ': ' + meaning + ')';
}

function spawnChild(name, command, commandArgs, options) {
  console.log('[' + name + '] ' + path.basename(command) + ' ' + commandArgs.join(' '));
  const child = spawn(command, commandArgs, {
    cwd: options.cwd ?? repoRoot,
    stdio: 'inherit',
    shell: false,
    env: { ...process.env, ...options.env },
  });
  child.on('error', (error) => console.error('[' + name + '] failed to start: ' + error.message));
  return child;
}

function run(name, command, commandArgs, options = {}) {
  const start = () => {
    const child = spawnChild(name, command, commandArgs, options);
    child.on('exit', (code, signal) => {
      if (stopping) return;
      console.error('[' + name + '] exited: ' + describeExit(code, signal));
      if (name === 'runtime') {
        // Without the runtime the stack has nothing to talk to: stop rather than leave a control
        // server and a dev server pointing at a dead gateway.
        console.error('[dev] the runtime is gone, stopping the rest of the stack');
        shutdown();
        return;
      }
      if (options.restart === false) return;
      // A crashed dev server used to leave a console that could not load anything. Restart it,
      // but never in a loop: three crashes a minute means something is actually wrong.
      const recent = (restarts.get(name) ?? []).filter((at) => Date.now() - at < 60_000);
      if (recent.length >= 3) {
        console.error(
          '[' + name + '] crashed ' + recent.length + ' times in the last minute; not restarting it.',
        );
        console.error('[dev] the rest of the stack is still running; fix the cause, then re-run pnpm dev');
        return;
      }
      recent.push(Date.now());
      restarts.set(name, recent);
      console.log('[' + name + '] restarting in 1s (crash ' + recent.length + ' of 3 allowed per minute)');
      setTimeout(start, 1_000);
    });
    children.set(name, child);
    return child;
  };
  return start();
}

/** pnpm keeps each dependency in its own directory, so resolve the bin from the package itself. */
function resolveViteBin() {
  const candidates = [
    path.join(repoRoot, 'apps', 'web', 'node_modules', 'vite', 'bin', 'vite.js'),
    path.join(repoRoot, 'node_modules', 'vite', 'bin', 'vite.js'),
  ];
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

function shutdown() {
  stopping = true;
  for (const [name, child] of children) {
    console.log('[dev] stopping ' + name);
    if (!child.killed) child.kill();
  }
  process.exit(0);
}

process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);

if (wantRuntime) {
  const binary = path.join(repoRoot, 'target', 'debug', isWindows ? 'agentos-server.exe' : 'agentos-server');
  const env = {
    AGENTOS_HTTP_ADDR: '127.0.0.1:' + runtimeHttp.value,
    AGENTOS_GRPC_ADDR: '127.0.0.1:' + runtimeGrpc.value,
    AGENTOS_NODE_NAME: nodeName,
  };
  if (existsSync(binary)) {
    run('runtime', binary, [], { env });
  } else {
    console.log('[dev] ' + binary + ' not found, building and running through cargo');
    run('runtime', isWindows ? 'cargo.exe' : 'cargo', ['run', '-p', 'agentos-server'], { env });
  }
}

if (!runtimeOnly && wantServer) {
  run('control', process.execPath, [path.join('apps', 'server', 'src', 'index.ts')], {
    env: {
      PORT: String(controlPort.value),
      RUNTIME_URL: process.env.RUNTIME_URL ?? runtimeUrl,
    },
  });
}

if (!runtimeOnly && wantWeb) {
  const viteBin = resolveViteBin();
  const env = { AGENTOS_HTTP_TARGET: process.env.AGENTOS_HTTP_TARGET ?? runtimeUrl };
  if (viteBin !== null) {
    run('web', process.execPath, [viteBin, '--port', String(webPort.value), '--strictPort'], {
      cwd: path.join(repoRoot, 'apps', 'web'),
      env,
    });
  } else {
    console.error('[dev] vite is not installed yet: run "pnpm install" first (or "pnpm --filter @agentos/web dev")');
  }
}

// --- banner -----------------------------------------------------------------------------------

const dataDir = process.env.AGENTOS_DATA_DIR ?? './data';
console.log('');
console.log('[dev] stack ready');
console.log('[dev]   runtime http : ' + runtimeUrl + '   (ws ' + runtimeUrl + '/v1/ws)');
console.log('[dev]   runtime grpc : 127.0.0.1:' + runtimeGrpc.value);
if (!runtimeOnly && wantServer) {
  console.log('[dev]   control      : http://127.0.0.1:' + controlPort.value + '   (ws /ws)');
}
if (!runtimeOnly && wantWeb) {
  console.log('[dev]   web          : http://localhost:' + webPort.value);
}
console.log('[dev]   data dir     : ' + path.resolve(repoRoot, dataDir) + '  (one per running stack)');
console.log('[dev]   node name    : ' + nodeName + '  (AGENTOS_NODE_NAME)');
console.log('[dev]   node id      : persisted in the data dir, unique per stack');
console.log('');