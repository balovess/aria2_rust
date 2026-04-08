import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { RpcError, ConnectionError, TimeoutError } from '../../src/errors.js';
import { skipIfNoBinary } from './helpers.js';

describe.skipIf(skipIfNoBinary())('Error Handling E2E', () => {
  it('invalid GID throws RpcError', async () => {
    const client = new Aria2Client('http://localhost:6800/jsonrpc');
    await expect(client.tellStatus('invalid_gid')).rejects.toThrow(RpcError);
    await client.close();
  });

  it('connection refused throws ConnectionError', async () => {
    const client = new Aria2Client('http://localhost:19999/jsonrpc', { timeout: 3000 });
    await expect(client.getVersion()).rejects.toThrow(ConnectionError);
    await client.close();
  });

  it('timeout throws TimeoutError', async () => {
    const client = new Aria2Client('http://localhost:6800/jsonrpc', { timeout: 1 });
    try {
      await client.getVersion();
      expect.unreachable('Should have thrown');
    } catch (err) {
      expect(err).toBeInstanceOf(TimeoutError);
    }
    await client.close();
  });
});
