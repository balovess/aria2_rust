import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { RpcError } from '../../src/errors.js';
import { startMockServer } from './helpers.js';

describe('RPC Methods Integration', () => {
  let client: Aria2Client;
  let serverUrl: string;
  let stop: () => Promise<void>;

  beforeAll(async () => {
    const result = await startMockServer();
    serverUrl = result.url;
    stop = result.stop;
    client = new Aria2Client(serverUrl);
  });

  afterAll(async () => {
    await client.close();
    await stop();
  });

  it('addUri returns GID', async () => {
    const gid = await client.addUri(['http://example.com/file.zip']);
    expect(gid).toBe('2089b05ecca3d829');
  });

  it('addTorrent returns GID', async () => {
    const torrent = Buffer.from('fake-torrent-data');
    const gid = await client.addTorrent(torrent);
    expect(gid).toBe('2089b05ecca3d829');
  });

  it('addMetalink returns GID', async () => {
    const metalink = Buffer.from('fake-metalink-data');
    const gid = await client.addMetalink(metalink);
    expect(gid).toBe('2089b05ecca3d829');
  });

  it('remove existing task', async () => {
    const gid = await client.remove('2089b05ecca3d829');
    expect(gid).toBe('2089b05ecca3d829');
  });

  it('remove nonexistent throws RpcError', async () => {
    const errServer = await startMockServer();
    const customClient = new Aria2Client(errServer.url);

    const origFetch = globalThis.fetch;
    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const body = JSON.parse(init!.body as string);
      return new Response(
        JSON.stringify({
          jsonrpc: '2.0',
          error: { code: 1, message: 'GID not found' },
          id: body.id,
        }),
        { status: 200, headers: { 'Content-Type': 'application/json' } },
      );
    };

    await expect(customClient.remove('nonexistent')).rejects.toThrow(RpcError);
    globalThis.fetch = origFetch;
    await customClient.close();
    await errServer.stop();
  });

  it('pause and unpause', async () => {
    const gid1 = await client.pause('2089b05ecca3d829');
    expect(gid1).toBe('2089b05ecca3d829');
    const gid2 = await client.unpause('2089b05ecca3d829');
    expect(gid2).toBe('2089b05ecca3d829');
  });

  it('tellStatus', async () => {
    const status = await client.tellStatus('2089b05ecca3d829');
    expect(status.gid).toBe('2089b05ecca3d829');
    expect(status.status).toBe('active');
  });

  it('tellActive', async () => {
    const list = await client.tellActive();
    expect(Array.isArray(list)).toBe(true);
    expect(list.length).toBeGreaterThan(0);
    expect(list[0].gid).toBe('2089b05ecca3d829');
  });

  it('tellWaiting', async () => {
    const list = await client.tellWaiting(0, 10);
    expect(Array.isArray(list)).toBe(true);
    expect(list[0].status).toBe('waiting');
  });

  it('tellStopped', async () => {
    const list = await client.tellStopped(0, 10);
    expect(Array.isArray(list)).toBe(true);
    expect(list[0].status).toBe('complete');
  });

  it('getGlobalStat', async () => {
    const stat = await client.getGlobalStat();
    expect(stat.downloadSpeed).toBe('20480');
    expect(stat.numActive).toBe('1');
  });

  it('getVersion', async () => {
    const version = await client.getVersion();
    expect(version.version).toBe('1.37.0');
    expect(Array.isArray(version.enabledFeatures)).toBe(true);
  });

  it('getSessionInfo', async () => {
    const session = await client.getSessionInfo();
    expect(session.sessionId).toBe('abc123');
  });

  it('shutdown', async () => {
    const result = await client.shutdown();
    expect(result).toBe('OK');
  });

  it('saveSession', async () => {
    const result = await client.saveSession();
    expect(result).toBe('OK');
  });

  it('purgeDownloadResult', async () => {
    const result = await client.purgeDownloadResult();
    expect(result).toBe('OK');
  });
});
