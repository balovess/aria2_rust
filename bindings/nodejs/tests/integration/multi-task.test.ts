import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { startMockServer } from './helpers.js';

describe('Multi-task Integration', () => {
  let client: Aria2Client;
  let stop: () => Promise<void>;

  beforeAll(async () => {
    const result = await startMockServer();
    client = new Aria2Client(result.url);
    stop = result.stop;
  });

  afterAll(async () => {
    await client.close();
    await stop();
  });

  it('add multiple tasks', async () => {
    const gid1 = await client.addUri(['http://example.com/file1.zip']);
    const gid2 = await client.addUri(['http://example.com/file2.zip']);
    expect(gid1).toBe('2089b05ecca3d829');
    expect(gid2).toBe('2089b05ecca3d829');
  });

  it('tellActive after adding', async () => {
    const list = await client.tellActive();
    expect(Array.isArray(list)).toBe(true);
    expect(list.length).toBeGreaterThan(0);
    expect(list[0].status).toBe('active');
  });

  it('pause/resume/remove flow', async () => {
    const gid = await client.addUri(['http://example.com/file3.zip']);
    const paused = await client.pause(gid);
    expect(paused).toBe(gid);
    const resumed = await client.unpause(gid);
    expect(resumed).toBe(gid);
    const removed = await client.remove(gid);
    expect(removed).toBe(gid);
  });
});
