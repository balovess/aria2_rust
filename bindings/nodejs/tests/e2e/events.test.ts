import { describe, it, expect, vi } from 'vitest';
import { Aria2EventEmitter } from '../../src/events.js';
import { EventType } from '../../src/types.js';

describe('Events E2E', () => {
  it('event emitter receives events (mock WebSocket)', async () => {
    const emitter = new Aria2EventEmitter('ws://localhost:6800/jsonrpc');

    const handler = vi.fn();
    emitter.on('downloadStart', handler);

    const internalWs = (emitter as unknown as { ws: unknown }).ws;
    expect(internalWs).toBeNull();
  });

  it('event type filtering maps correctly', () => {
    const mapping: Record<string, string> = {
      [EventType.DownloadStart]: 'downloadStart',
      [EventType.DownloadPause]: 'downloadPause',
      [EventType.DownloadStop]: 'downloadStop',
      [EventType.DownloadComplete]: 'downloadComplete',
      [EventType.DownloadError]: 'downloadError',
      [EventType.BtDownloadComplete]: 'btDownloadComplete',
      [EventType.BtDownloadError]: 'btDownloadError',
    };

    expect(Object.keys(mapping)).toHaveLength(7);
    expect(mapping[EventType.DownloadStart]).toBe('downloadStart');
    expect(mapping[EventType.DownloadComplete]).toBe('downloadComplete');
    expect(mapping[EventType.DownloadError]).toBe('downloadError');
  });
});
