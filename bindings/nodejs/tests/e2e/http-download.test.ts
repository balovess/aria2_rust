import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { skipIfNoBinary, startFileServer } from './helpers.js';

describe.skipIf(skipIfNoBinary())('HTTP Download E2E', () => {
  let client: Aria2Client;
  let fileServer: { url: string; stop: () => Promise<void> };

  beforeAll(async () => {
    fileServer = await startFileServer();
    client = new Aria2Client('http://localhost:6800/jsonrpc');
  });

  afterAll(async () => {
    await client.close();
    await fileServer.stop();
  });

  it('addUri and check status', async () => {
    const gid = await client.addUri([`${fileServer.url}/testfile.bin`]);
    expect(gid).toBeTruthy();
    const status = await client.tellStatus(gid);
    expect(status.gid).toBe(gid);
  });

  it('tellStatus progress', async () => {
    const gid = await client.addUri([`${fileServer.url}/testfile.bin`]);
    const status = await client.tellStatus(gid, [
      'gid',
      'status',
      'totalLength',
      'completedLength',
    ]);
    expect(status.gid).toBe(gid);
    expect(['active', 'waiting', 'complete', 'paused']).toContain(status.status);
  });

  it('download complete status', async () => {
    const gid = await client.addUri([`${fileServer.url}/testfile.bin`]);
    await new Promise((r) => setTimeout(r, 2000));
    const status = await client.tellStatus(gid);
    expect(['complete', 'active', 'waiting']).toContain(status.status);
  });
});
