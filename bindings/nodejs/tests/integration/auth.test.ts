import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { RpcError } from '../../src/errors.js';
import { startMockServer } from './helpers.js';

describe('Auth Integration', () => {
  it('token auth success', async () => {
    const { url, stop } = await startMockServer({ token: 'mysecret' });
    const client = new Aria2Client(url, { token: 'mysecret' });
    const version = await client.getVersion();
    expect(version.version).toBe('1.37.0');
    await client.close();
    await stop();
  });

  it('token auth failure', async () => {
    const { url, stop } = await startMockServer({ token: 'mysecret' });
    const client = new Aria2Client(url, { token: 'wrongtoken' });
    await expect(client.getVersion()).rejects.toThrow(RpcError);
    await client.close();
    await stop();
  });

  it('no auth when no token configured', async () => {
    const { url, stop } = await startMockServer();
    const client = new Aria2Client(url);
    const version = await client.getVersion();
    expect(version.version).toBe('1.37.0');
    await client.close();
    await stop();
  });
});
