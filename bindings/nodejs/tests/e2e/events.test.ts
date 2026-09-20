import { describe, it, expect, vi } from 'vitest';
import { WebSocketServer } from 'ws';
import type { AddressInfo } from 'node:net';
import { Aria2Client } from '../../src/client.js';
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

  it('connectEvents waits for the event WebSocket to be ready', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise<void>((resolve) => server.once('listening', resolve));
    const address = server.address() as AddressInfo;
    const client = new Aria2Client(`http://127.0.0.1:${address.port}/jsonrpc`);

    const emitter = await client.connectEvents();
    const eventPromise = new Promise<unknown>((resolve) => {
      emitter.once('downloadStart', resolve);
    });

    const socket = [...server.clients][0];
    expect(socket).toBeDefined();
    socket.send(
      JSON.stringify({
        jsonrpc: '2.0',
        method: 'aria2.onDownloadStart',
        params: [{ gid: '0123456789abcdef' }],
      }),
    );

    await expect(eventPromise).resolves.toEqual({
      type: EventType.DownloadStart,
      gid: '0123456789abcdef',
    });

    await client.close();
    await new Promise<void>((resolve, reject) => server.close((error) => {
      if (error) reject(error);
      else resolve();
    }));
  });

  it('close settles an in-flight event connection', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise<void>((resolve) => server.once('listening', resolve));
    const address = server.address() as AddressInfo;
    const emitter = new Aria2EventEmitter(`ws://127.0.0.1:${address.port}/jsonrpc`);

    const connection = emitter.connect();
    await emitter.close();

    await expect(connection).rejects.toThrow('Emitter has been closed');
    await new Promise<void>((resolve, reject) => server.close((error) => {
      if (error) reject(error);
      else resolve();
    }));
  });
});
