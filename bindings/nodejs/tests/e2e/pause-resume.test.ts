import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { skipIfNoBinary, startFileServer, startAria2Server } from './helpers.js';

describe.skipIf(skipIfNoBinary())('Pause/Resume E2E', () => {
  let client: Aria2Client;
  let fileServer: { url: string; stop: () => Promise<void> };
  let aria2Server: { url: string; stop: () => Promise<void> };

  beforeAll(async () => {
    aria2Server = await startAria2Server();
    fileServer = await startFileServer();
    client = new Aria2Client(aria2Server.url);
  });

  afterAll(async () => {
    await client.close();
    await fileServer.stop();
    await aria2Server.stop();
  });

  it('pause download', async () => {
    const gid = await client.addUri([`${fileServer.url}/testfile.bin`]);
    const result = await client.pause(gid);
    expect(result).toBe(gid);
    const status = await client.tellStatus(gid);
    expect(['paused', 'active', 'complete']).toContain(status.status);
  });

  it('unpause download', async () => {
    const gid = await client.addUri([`${fileServer.url}/testfile.bin`]);
    await client.pause(gid);
    const result = await client.unpause(gid);
    expect(result).toBe(gid);
    const status = await client.tellStatus(gid);
    expect(['active', 'waiting', 'complete']).toContain(status.status);
  });
});
