import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { skipIfNoBinary, startAria2Server } from './helpers.js';

describe.skipIf(skipIfNoBinary())('Concurrent E2E', () => {
  let client: Aria2Client;
  let aria2Server: { url: string; stop: () => Promise<void> };

  beforeAll(async () => {
    aria2Server = await startAria2Server();
    client = new Aria2Client(aria2Server.url);
  });

  afterAll(async () => {
    await client.close();
    await aria2Server.stop();
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
