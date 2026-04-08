import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import { startMockServer } from './helpers.js';

describe('Options Integration', () => {
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

  it('getGlobalOption', async () => {
    const opts = await client.getGlobalOption();
    expect(opts['max-overall-download-limit']).toBe('0');
  });

  it('changeGlobalOption', async () => {
    const result = await client.changeGlobalOption({
      'max-overall-download-limit': '1M',
    });
    expect(result).toBe('OK');
  });

  it('getOption for task', async () => {
    const opts = await client.getOption('2089b05ecca3d829');
    expect(opts.dir).toBe('/tmp');
  });

  it('changeOption for task', async () => {
    const result = await client.changeOption('2089b05ecca3d829', {
      'max-download-limit': '512K',
    });
    expect(result).toBe('OK');
  });
});
