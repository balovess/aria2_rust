import { createServer, type Server } from 'http';
import { execSync, spawn, type ChildProcess } from 'child_process';
import path from 'path';
import fs from 'fs';

const BINARY_NAMES = ['aria2c-rust', 'aria2c'];
const RPC_PORT = 6800;

export function findBinary(): string | null {
  for (const name of BINARY_NAMES) {
    try {
      execSync(`${name} --version`, { stdio: 'ignore' });
      return name;
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
  const testDir = path.join(process.cwd(), 'tests', 'e2e', 'fixtures');
  if (!fs.existsSync(testDir)) {
    fs.mkdirSync(testDir, { recursive: true });
    fs.writeFileSync(path.join(testDir, 'testfile.bin'), Buffer.alloc(1024, 'A'));
  }

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

export async function startAria2Server(): Promise<Aria2ServerResult> {
  const binary = findBinary();
  if (!binary) {
    throw new Error('aria2 binary not found. Please install aria2c or aria2c-rust.');
  }

  // Create a temp directory for aria2 downloads and session
  const baseTempDir = fs.mkdtempSync(path.join(process.cwd(), 'aria2-test-'));
  const tempDir = path.join(baseTempDir, 'download');
  fs.mkdirSync(tempDir, { recursive: true });

  const aria2Process = spawn(
    binary,
    [
      '--enable-rpc=true',
      `--rpc-listen-port=${RPC_PORT}`,
      '--rpc-listen-all=false',
      '--rpc-allow-origin-all=true',
      `--dir=${tempDir}`,
      `--save-session=${path.join(baseTempDir, 'aria2.session')}`,
      '--input-file=',
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

  while (Date.now() - startTime < maxWaitTime) {
    try {
      const response = await fetch(`http://localhost:${RPC_PORT}/jsonrpc`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0',
          method: 'aria2.getVersion',
          id: 'test',
        }),
      });
      if (response.ok) {
        break;
      }
    } catch (err) {
      lastError = err instanceof Error ? err : new Error(String(err));
    }
    await new Promise((r) => setTimeout(r, 100));
  }

  // Check if we successfully connected
  try {
    await fetch(`http://localhost:${RPC_PORT}/jsonrpc`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        method: 'aria2.getVersion',
        id: 'test',
      }),
    });
  } catch (err) {
    aria2Process.kill();
    throw new Error(
      `Failed to start aria2 RPC server within ${maxWaitTime}ms: ${lastError?.message ?? 'unknown error'}`,
    );
  }

  return {
    url: `http://localhost:${RPC_PORT}/jsonrpc`,
    stop: async () => {
      // Try graceful shutdown via RPC first
      try {
        await fetch(`http://localhost:${RPC_PORT}/jsonrpc`, {
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
      if (aria2Process.pid) {
        try {
          process.kill(aria2Process.pid, 'SIGTERM');
        } catch {
          // Process may have already exited
        }
      }

      // Wait for process to exit
      await new Promise<void>((resolve) => {
        aria2Process.on('exit', () => resolve());
        aria2Process.on('error', () => resolve());
        // Force resolve after timeout
        setTimeout(resolve, 2000);
      });

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
