import { describe, it, expect, vi, beforeEach } from 'vitest';
import { Aria2Client } from '../../src/client.js';
import type { Transport } from '../../src/transport.js';

function createMockTransport(): Transport & { sendRequest: ReturnType<typeof vi.fn> } {
  return {
    sendRequest: vi.fn(),
    close: vi.fn().mockResolvedValue(undefined),
  };
}

describe('Aria2Client', () => {
  let client: Aria2Client;
  let mockTransport: ReturnType<typeof createMockTransport>;

  beforeEach(() => {
    mockTransport = createMockTransport();
    client = new Aria2Client('http://localhost:6800/jsonrpc');
    (client as unknown as { transport: Transport }).transport = mockTransport;
  });

  describe('addUri', () => {
    it('sends correct method and params', async () => {
      mockTransport.sendRequest.mockResolvedValue('2089b05ecca3d829');
      const gid = await client.addUri(['http://example.com/file.zip']);
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.addUri', [
        ['http://example.com/file.zip'],
      ]);
      expect(gid).toBe('2089b05ecca3d829');
    });

    it('sends with options', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      await client.addUri(['http://example.com/file.zip'], { dir: '/tmp' });
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.addUri', [
        ['http://example.com/file.zip'],
        { dir: '/tmp' },
      ]);
    });
  });

  describe('addTorrent', () => {
    it('base64-encodes torrent Buffer', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      const torrent = Buffer.from('torrent-data');
      await client.addTorrent(torrent);
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.addTorrent', [
        torrent.toString('base64'),
      ]);
    });

    it('sends with options', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      const torrent = Buffer.from('torrent-data');
      await client.addTorrent(torrent, { dir: '/tmp' });
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.addTorrent', [
        torrent.toString('base64'),
        { dir: '/tmp' },
      ]);
    });
  });

  describe('addMetalink', () => {
    it('base64-encodes metalink Buffer', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      const metalink = Buffer.from('metalink-data');
      await client.addMetalink(metalink);
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.addMetalink', [
        metalink.toString('base64'),
      ]);
    });
  });

  describe('task control methods', () => {
    it('remove sends correct method', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      await client.remove('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.remove', ['gid1']);
    });

    it('pause sends correct method', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      await client.pause('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.pause', ['gid1']);
    });

    it('unpause sends correct method', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      await client.unpause('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.unpause', ['gid1']);
    });

    it('forcePause sends correct method', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      await client.forcePause('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.forcePause', ['gid1']);
    });

    it('forceRemove sends correct method', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      await client.forceRemove('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.forceRemove', ['gid1']);
    });

    it('forceUnpause sends correct method', async () => {
      mockTransport.sendRequest.mockResolvedValue('gid1');
      await client.forceUnpause('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.forceUnpause', ['gid1']);
    });
  });

  describe('tellStatus', () => {
    it('returns StatusInfo', async () => {
      const statusInfo = {
        gid: 'gid1',
        status: 'active',
        totalLength: '1024',
      };
      mockTransport.sendRequest.mockResolvedValue(statusInfo);
      const result = await client.tellStatus('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.tellStatus', ['gid1']);
      expect(result).toEqual(statusInfo);
    });

    it('sends with keys', async () => {
      mockTransport.sendRequest.mockResolvedValue({ gid: 'gid1', status: 'active' });
      await client.tellStatus('gid1', ['gid', 'status']);
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.tellStatus', [
        'gid1',
        ['gid', 'status'],
      ]);
    });
  });

  describe('tellActive/tellWaiting/tellStopped', () => {
    it('tellActive returns StatusInfo[]', async () => {
      const items = [{ gid: 'gid1', status: 'active' }];
      mockTransport.sendRequest.mockResolvedValue(items);
      const result = await client.tellActive();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.tellActive', []);
      expect(result).toEqual(items);
    });

    it('tellWaiting returns StatusInfo[]', async () => {
      const items = [{ gid: 'gid2', status: 'waiting' }];
      mockTransport.sendRequest.mockResolvedValue(items);
      const result = await client.tellWaiting(0, 10);
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.tellWaiting', [0, 10]);
      expect(result).toEqual(items);
    });

    it('tellStopped returns StatusInfo[]', async () => {
      const items = [{ gid: 'gid3', status: 'complete' }];
      mockTransport.sendRequest.mockResolvedValue(items);
      const result = await client.tellStopped(0, 10);
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.tellStopped', [0, 10]);
      expect(result).toEqual(items);
    });
  });

  describe('getGlobalStat', () => {
    it('returns GlobalStat', async () => {
      const stat = {
        downloadSpeed: '20480',
        uploadSpeed: '0',
        numActive: '1',
        numWaiting: '0',
        numStopped: '0',
        numStoppedTotal: '0',
      };
      mockTransport.sendRequest.mockResolvedValue(stat);
      const result = await client.getGlobalStat();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.getGlobalStat', []);
      expect(result).toEqual(stat);
    });
  });

  describe('getVersion', () => {
    it('returns VersionInfo', async () => {
      const version = { version: '1.37.0', enabledFeatures: ['Async DNS'] };
      mockTransport.sendRequest.mockResolvedValue(version);
      const result = await client.getVersion();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.getVersion', []);
      expect(result).toEqual(version);
    });
  });

  describe('getSessionInfo', () => {
    it('returns SessionInfo', async () => {
      const session = { sessionId: 'abc123' };
      mockTransport.sendRequest.mockResolvedValue(session);
      const result = await client.getSessionInfo();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.getSessionInfo', []);
      expect(result).toEqual(session);
    });
  });

  describe('getGlobalOption / changeGlobalOption', () => {
    it('getGlobalOption returns options', async () => {
      const opts = { 'max-overall-download-limit': '0' };
      mockTransport.sendRequest.mockResolvedValue(opts);
      const result = await client.getGlobalOption();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.getGlobalOption', []);
      expect(result).toEqual(opts);
    });

    it('changeGlobalOption sends options', async () => {
      mockTransport.sendRequest.mockResolvedValue('OK');
      await client.changeGlobalOption({ 'max-overall-download-limit': '1M' });
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.changeGlobalOption', [
        { 'max-overall-download-limit': '1M' },
      ]);
    });
  });

  describe('getOption / changeOption', () => {
    it('getOption for task', async () => {
      const opts = { dir: '/tmp' };
      mockTransport.sendRequest.mockResolvedValue(opts);
      const result = await client.getOption('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.getOption', ['gid1']);
      expect(result).toEqual(opts);
    });

    it('changeOption for task', async () => {
      mockTransport.sendRequest.mockResolvedValue('OK');
      await client.changeOption('gid1', { 'max-download-limit': '512K' });
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.changeOption', [
        'gid1',
        { 'max-download-limit': '512K' },
      ]);
    });
  });

  describe('purgeDownloadResult / removeDownloadResult', () => {
    it('purgeDownloadResult', async () => {
      mockTransport.sendRequest.mockResolvedValue('OK');
      await client.purgeDownloadResult();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.purgeDownloadResult', []);
    });

    it('removeDownloadResult', async () => {
      mockTransport.sendRequest.mockResolvedValue('OK');
      await client.removeDownloadResult('gid1');
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.removeDownloadResult', [
        'gid1',
      ]);
    });
  });

  describe('shutdown / forceShutdown / saveSession', () => {
    it('shutdown', async () => {
      mockTransport.sendRequest.mockResolvedValue('OK');
      await client.shutdown();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.shutdown', []);
    });

    it('forceShutdown', async () => {
      mockTransport.sendRequest.mockResolvedValue('OK');
      await client.forceShutdown();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.forceShutdown', []);
    });

    it('saveSession', async () => {
      mockTransport.sendRequest.mockResolvedValue('OK');
      await client.saveSession();
      expect(mockTransport.sendRequest).toHaveBeenCalledWith('aria2.saveSession', []);
    });
  });

  describe('close', () => {
    it('calls transport close', async () => {
      await client.close();
      expect(mockTransport.close).toHaveBeenCalledOnce();
    });
  });

  describe('destroy', () => {
    it('calls transport close without awaiting', () => {
      client.destroy();
      expect(mockTransport.close).toHaveBeenCalledOnce();
    });
  });
});
