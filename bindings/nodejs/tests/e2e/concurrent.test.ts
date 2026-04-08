import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { skipIfNoBinary } from './helpers.js';

describe.skipIf(skipIfNoBinary())('Concurrent E2E', () => {
  let client: Aria2Client;

  beforeAll(() => {
    client = new Aria2Client('http://localhost:6800/jsonrpc');
  });

  afterAll(async () => {
    await client.close();
  });

  it('concurrent addUri (10 concurrent requests)', async () => {
    const promises = Array.from({ length: 10 }, (_, i) =>
      client.addUri([`http://example.com/file${i}.zip`]),
    );
    const gids = await Promise.all(promises);
    expect(gids).toHaveLength(10);
    gids.forEach((gid) => expect(gid).toBeTruthy());
  });
});
