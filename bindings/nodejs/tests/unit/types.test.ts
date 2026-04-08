import { describe, it, expect } from 'vitest';
import { EventType, DownloadStatus } from '../../src/types.js';
import type {
  StatusInfo,
  GlobalStat,
  VersionInfo,
  SessionInfo,
  ClientOptions,
  FileInfo,
  UriEntry,
  DownloadEvent,
} from '../../src/types.js';

describe('EventType', () => {
  it('has DownloadStart', () => {
    expect(EventType.DownloadStart).toBe('aria2.onDownloadStart');
  });

  it('has DownloadPause', () => {
    expect(EventType.DownloadPause).toBe('aria2.onDownloadPause');
  });

  it('has DownloadStop', () => {
    expect(EventType.DownloadStop).toBe('aria2.onDownloadStop');
  });

  it('has DownloadComplete', () => {
    expect(EventType.DownloadComplete).toBe('aria2.onDownloadComplete');
  });

  it('has DownloadError', () => {
    expect(EventType.DownloadError).toBe('aria2.onDownloadError');
  });

  it('has BtDownloadComplete', () => {
    expect(EventType.BtDownloadComplete).toBe('aria2.onBtDownloadComplete');
  });

  it('has BtDownloadError', () => {
    expect(EventType.BtDownloadError).toBe('aria2.onBtDownloadError');
  });

  it('has exactly 7 values', () => {
    const values = Object.values(EventType);
    expect(values).toHaveLength(7);
  });
});

describe('DownloadStatus', () => {
  it('has Active', () => {
    expect(DownloadStatus.Active).toBe('active');
  });

  it('has Waiting', () => {
    expect(DownloadStatus.Waiting).toBe('waiting');
  });

  it('has Paused', () => {
    expect(DownloadStatus.Paused).toBe('paused');
  });

  it('has Error', () => {
    expect(DownloadStatus.Error).toBe('error');
  });

  it('has Complete', () => {
    expect(DownloadStatus.Complete).toBe('complete');
  });

  it('has Removed', () => {
    expect(DownloadStatus.Removed).toBe('removed');
  });

  it('has exactly 6 values', () => {
    const values = Object.values(DownloadStatus);
    expect(values).toHaveLength(6);
  });
});

describe('StatusInfo', () => {
  it('can create a valid StatusInfo object', () => {
    const info: StatusInfo = {
      gid: '2089b05ecca3d829',
      status: DownloadStatus.Active,
      totalLength: '34896136',
      completedLength: '34896136',
      downloadSpeed: '0',
      uploadSpeed: '0',
    };
    expect(info.gid).toBe('2089b05ecca3d829');
    expect(info.status).toBe('active');
  });

  it('supports optional fields', () => {
    const info: StatusInfo = {
      gid: 'abc',
      status: DownloadStatus.Waiting,
    };
    expect(info.errorCode).toBeUndefined();
    expect(info.files).toBeUndefined();
  });
});

describe('GlobalStat', () => {
  it('can create a valid GlobalStat object', () => {
    const stat: GlobalStat = {
      downloadSpeed: '20480',
      uploadSpeed: '0',
      numActive: '1',
      numWaiting: '0',
      numStopped: '0',
      numStoppedTotal: '0',
    };
    expect(stat.downloadSpeed).toBe('20480');
    expect(stat.numActive).toBe('1');
  });
});

describe('VersionInfo', () => {
  it('can create a valid VersionInfo object', () => {
    const info: VersionInfo = {
      version: '1.37.0',
      enabledFeatures: ['Async DNS', 'BitTorrent'],
    };
    expect(info.version).toBe('1.37.0');
    expect(info.enabledFeatures).toHaveLength(2);
  });
});

describe('SessionInfo', () => {
  it('can create a valid SessionInfo object', () => {
    const info: SessionInfo = {
      sessionId: 'abc123',
    };
    expect(info.sessionId).toBe('abc123');
  });
});

describe('ClientOptions', () => {
  it('can create empty options', () => {
    const opts: ClientOptions = {};
    expect(opts.token).toBeUndefined();
    expect(opts.timeout).toBeUndefined();
  });

  it('can create options with all fields', () => {
    const opts: ClientOptions = {
      token: 'mytoken',
      timeout: 5000,
      secret: 'mysecret',
    };
    expect(opts.token).toBe('mytoken');
    expect(opts.timeout).toBe(5000);
    expect(opts.secret).toBe('mysecret');
  });
});

describe('FileInfo', () => {
  it('can create a valid FileInfo object', () => {
    const file: FileInfo = {
      index: '1',
      path: '/tmp/file.bin',
      length: '34896136',
      completedLength: '34896136',
      selected: 'true',
      uris: [{ uri: 'http://example.com/file.bin', status: 'used' }],
    };
    expect(file.uris).toHaveLength(1);
    expect(file.uris[0].status).toBe('used');
  });
});

describe('DownloadEvent', () => {
  it('can create a valid DownloadEvent object', () => {
    const event: DownloadEvent = {
      type: EventType.DownloadStart,
      gid: '2089b05ecca3d829',
    };
    expect(event.type).toBe('aria2.onDownloadStart');
    expect(event.gid).toBe('2089b05ecca3d829');
  });
});
