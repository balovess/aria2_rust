import { createServer, type Server } from 'http';
import { createServer as createTcpServer } from 'net';
import { execFileSync, spawn, type ChildProcess } from 'child_process';
import path from 'path';
import fs from 'fs';
import os from 'os';

const BINARY_NAMES = ['aria2c-rust', 'aria2_rust', 'aria2c'];

function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createTcpServer();
    probe.once('error', reject);
    probe.listen(0, '127.0.0.1', () => {
      const address = probe.address();
      if (!address || typeof address === 'string') {
        probe.close();
        reject(new Error('Could not determine an ephemeral TCP port'));
        return;
      }
      const port = address.port;
      probe.close((error) => (error ? reject(error) : resolve(port)));
    });
  });
}

export function findBinary(): string | null {
  const candidates: string[] = [];
  if (process.env.ARIA2_RUST_BIN) {
    candidates.push(process.env.ARIA2_RUST_BIN);
  }

  const roots = [
    process.cwd(),
    path.resolve(process.cwd(), '..'),
    path.resolve(process.cwd(), '..', '..'),
    path.resolve(process.cwd(), '..', '..', '..'),
  ];
  for (const root of roots) {
    for (const targetDir of ['target/debug', 'target-check/debug']) {
      for (const name of ['aria2c', 'aria2c.exe', 'aria2c-rust', 'aria2c-rust.exe']) {
        candidates.push(path.join(root, targetDir, name));
      }
    }
  }

  for (const name of BINARY_NAMES) {
    candidates.push(name);
  }

  for (const candidate of [...new Set(candidates)]) {
    try {
      execFileSync(candidate, ['--version'], { stdio: 'ignore' });
      return candidate;
    } catch {
      continue;
    }
  }
  return null;
}

export function isBinaryAvailable(): boolean {
  return findBinary() !== null;
}

export function skipIfNoBinary(): boolean {
  return !isBinaryAvailable();
}

export async function startFileServer(): Promise<{ url: string; stop: () => Promise<void> }> {
  const testDir = fs.mkdtempSync(path.join(os.tmpdir(), 'aria2-node-e2e-files-'));
  fs.writeFileSync(path.join(testDir, 'testfile.bin'), Buffer.alloc(1024, 'A'));

  const server: Server = createServer((req, res) => {
    const filePath = path.join(testDir, path.basename(req.url ?? 'testfile.bin'));
    if (fs.existsSync(filePath)) {
      res.writeHead(200, { 'Content-Type': 'application/octet-stream' });
      fs.createReadStream(filePath).pipe(res);
    } else {
      res.writeHead(404);
      res.end('Not found');
    }
  });

  return new Promise((resolve) => {
    server.listen(0, () => {
      const addr = server.address();
      const port = typeof addr === 'object' && addr ? addr.port : 8080;
      resolve({
        url: `http://localhost:${port}`,
        stop: () =>
          new Promise<void>((res, rej) => {
            server.close((err) => {
              fs.rmSync(testDir, { recursive: true, force: true });
              err ? rej(err) : res();
            });
          }),
      });
    });
  });
}

export async function startDelayedRpcServer(
  delayMs = 100,
): Promise<{ url: string; stop: () => Promise<void> }> {
  const server: Server = createServer((req, res) => {
    req.resume();
    req.once('end', () => {
      setTimeout(() => {
        if (res.destroyed) {
          return;
        }
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(
          JSON.stringify({
            jsonrpc: '2.0',
            result: { version: '0.3.1' },
            id: 1,
          }),
        );
      }, delayMs);
    });
  });

  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address();
      const port = typeof addr === 'object' && addr ? addr.port : 8080;
      resolve({
        url: 'http://127.0.0.1:' + port + '/jsonrpc',
        stop: () =>
          new Promise<void>((res, rej) => {
            server.close((err) => (err ? rej(err) : res()));
          }),
      });
    });
  });
}

export interface Aria2ServerResult {
  url: string;
  stop: () => Promise<void>;
  process: ChildProcess;
}

async function waitForProcessExit(child: ChildProcess, timeoutMs: number): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return;
  }

  await new Promise<void>((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timeout);
      child.removeListener('exit', finish);
      child.removeListener('close', finish);
      child.removeListener('error', finish);
      resolve();
    };

    const timeout = setTimeout(finish, timeoutMs);
    child.once('exit', finish);
    child.once('close', finish);
    child.once('error', finish);
  });
}

export async function startAria2Server(): Promise<Aria2ServerResult> {
  const binary = findBinary();
  if (!binary) {
    throw new Error('aria2 binary not found. Please install aria2c or aria2c-rust.');
  }

  const rpcPort = await findFreePort();

  // Create a temp directory for aria2 downloads and session
  const baseTempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'aria2-node-e2e-'));
  const tempDir = path.join(baseTempDir, 'download');
  fs.mkdirSync(tempDir, { recursive: true });

  const aria2Process = spawn(
    binary,
    [
      '--enable-rpc=true',
      `--rpc-listen-port=${rpcPort}`,
      '--rpc-listen-all=false',
      '--rpc-allow-origin-all=true',
      `--dir=${tempDir}`,
      `--save-session=${path.join(baseTempDir, 'aria2.session')}`,
      '--continue=true',
      '--max-concurrent-downloads=5',
      '--max-connection-per-server=5',
      '--min-split-size=1M',
      '--split=5',
    ],
    {
      stdio: ['ignore', 'pipe', 'pipe'],
      detached: false,
    },
  );

  // Wait for the RPC server to be ready
  const maxWaitTime = 10000;
  const startTime = Date.now();
  let lastError: Error | null = null;
  let ready = false;
  const debugOutput = process.env.ARIA2_NODE_E2E_DEBUG === '1';
  const stdout: string[] = [];
  const stderr: string[] = [];
  aria2Process.stdout?.setEncoding('utf8');
  aria2Process.stdout?.on('data', (chunk: string) => {
    stdout.push(chunk);
    if (debugOutput) {
      process.stdout.write(`[aria2 stdout] ${chunk}`);
    }
  });
  aria2Process.stderr?.setEncoding('utf8');
  aria2Process.stderr?.on('data', (chunk: string) => {
    stderr.push(chunk);
    if (debugOutput) {
      process.stderr.write(`[aria2 stderr] ${chunk}`);
    }
  });
  aria2Process.once('exit', (code, signal) => {
    if (code !== 0 && code !== null) {
      process.stderr.write(
        `aria2 process exited unexpectedly (code=${code}, signal=${signal ?? 'none'}): ${stderr.join('').trim()}\n`,
      );
    }
  });

  while (Date.now() - startTime < maxWaitTime) {
    try {
      const response = await fetch(`http://127.0.0.1:${rpcPort}/jsonrpc`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0',
          method: 'aria2.getVersion',
          id: 'test',
        }),
      });
      if (response.ok) {
        ready = true;
        break;
      }
    } catch (err) {
      lastError = err instanceof Error ? err : new Error(String(err));
    }
    await new Promise((r) => setTimeout(r, 100));
  }

  if (!ready) {
    aria2Process.kill();
    await waitForProcessExit(aria2Process, 2000);
    fs.rmSync(baseTempDir, { recursive: true, force: true });
    throw new Error(
      `Failed to start aria2 RPC server within ${maxWaitTime}ms: ${
        stderr.join('').trim() || lastError?.message || 'unknown error'
      }`,
    );
  }

  return {
    url: `http://127.0.0.1:${rpcPort}/jsonrpc`,
    stop: async () => {
      // Try graceful shutdown via RPC first
      try {
        await fetch(`http://127.0.0.1:${rpcPort}/jsonrpc`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            jsonrpc: '2.0',
            method: 'aria2.shutdown',
            id: 'shutdown',
          }),
        });
      } catch {
        // Ignore shutdown errors
      }

      // Force kill if still running
      if (aria2Process.exitCode === null && aria2Process.signalCode === null) {
        try {
          aria2Process.kill('SIGTERM');
        } catch {
          // Process may have already exited
        }
      }

      // Wait for process to exit
      await waitForProcessExit(aria2Process, 2000);

      // Cleanup temp directory
      try {
        fs.rmSync(baseTempDir, { recursive: true, force: true });
      } catch {
        // Ignore cleanup errors
      }
    },
    process: aria2Process,
  };
}
