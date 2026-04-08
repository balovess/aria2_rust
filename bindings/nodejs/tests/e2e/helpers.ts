import { createServer, type Server } from 'http';
import { execSync } from 'child_process';
import path from 'path';
import fs from 'fs';

const BINARY_NAMES = ['aria2c-rust', 'aria2c'];

export function isBinaryAvailable(): boolean {
  for (const name of BINARY_NAMES) {
    try {
      execSync(`${name} --version`, { stdio: 'ignore' });
      return true;
    } catch {
      continue;
    }
  }
  return false;
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
