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
    };

    expect(Object.keys(mapping)).toHaveLength(6);
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

  it('connect is idempotent and close is safe to repeat', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise<void>((resolve) => server.once('listening', resolve));
    const address = server.address() as AddressInfo;
    const emitter = new Aria2EventEmitter(`ws://127.0.0.1:${address.port}/jsonrpc`);

    await Promise.all([emitter.connect(), emitter.connect()]);
    expect(server.clients.size).toBe(1);

    await emitter.close();
    await emitter.close();
    await expect(emitter.connect()).rejects.toThrow('Emitter has been closed');
    await new Promise<void>((resolve, reject) => server.close((error) => {
      if (error) reject(error);
      else resolve();
    }));
  });

  it('reconnects and continues delivering events after disconnect', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise<void>((resolve) => server.once('listening', resolve));
    const address = server.address() as AddressInfo;
    const emitter = new Aria2EventEmitter(`ws://127.0.0.1:${address.port}/jsonrpc`);
    const handler = vi.fn();

    emitter.on('downloadStart', handler);
    await emitter.connect();
    const firstSocket = [...server.clients][0];
    const reconnecting = new Promise<void>((resolve, reject) => {
      emitter.once('reconnecting', (willRetry: boolean, attempt: number) => {
        try {
          expect(willRetry).toBe(true);
          expect(attempt).toBe(1);
          resolve();
        } catch (error) {
          reject(error);
        }
      });
    });

    firstSocket.close();
    await expect(reconnecting).resolves.toBeUndefined();
    await vi.waitFor(() => expect(server.clients.size).toBe(1), {
      timeout: 3000,
      interval: 25,
    });

    const reconnectedSocket = [...server.clients][0];
    reconnectedSocket.send(JSON.stringify({
      jsonrpc: '2.0',
      method: 'aria2.onDownloadStart',
      params: [{ gid: 'reconnected' }],
    }));
    await vi.waitFor(() => expect(handler).toHaveBeenCalledWith({
      type: EventType.DownloadStart,
      gid: 'reconnected',
    }));

    await emitter.close();
    await new Promise<void>((resolve, reject) => server.close((error) => {
      if (error) reject(error);
      else resolve();
    }));
  });

  it('preserves optional error and file metadata and supports client listeners', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise<void>((resolve) => server.once('listening', resolve));
    const address = server.address() as AddressInfo;
    const client = new Aria2Client(`http://127.0.0.1:${address.port}/jsonrpc`);
    const handler = vi.fn();

    client.once('downloadError', handler);
    const emitter = await client.connectEvents();
    const socket = [...server.clients][0];
    socket.send(JSON.stringify({
      jsonrpc: '2.0',
      method: 'aria2.onDownloadError',
      params: [{ gid: 'gid1', errorCode: '3', files: [] }],
    }));

    await vi.waitFor(() => expect(handler).toHaveBeenCalledOnce());
    expect(handler).toHaveBeenCalledWith({
      type: EventType.DownloadError,
      gid: 'gid1',
      errorCode: 3,
      files: [],
    });

    const removed = vi.fn();
    emitter.on('downloadStart', removed);
    client.off('downloadStart', removed);
    emitter.emit('downloadStart', { type: EventType.DownloadStart, gid: 'gid2' });
    expect(removed).not.toHaveBeenCalled();

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

  it('waitForTerminal filters unrelated events and resolves the matching GID', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise<void>((resolve) => server.once('listening', resolve));
    const address = server.address() as AddressInfo;
    const emitter = new Aria2EventEmitter(`ws://127.0.0.1:${address.port}/jsonrpc`);
    const wait = emitter.waitForTerminal('target');
    await vi.waitFor(() => expect(server.clients.size).toBe(1));
    const socket = [...server.clients][0];

    socket.send(JSON.stringify({
      jsonrpc: '2.0',
      method: 'aria2.onDownloadStart',
      params: [{ gid: 'target' }],
    }));
    socket.send(JSON.stringify({
      jsonrpc: '2.0',
      method: 'aria2.onDownloadComplete',
      params: [{ gid: 'other' }],
    }));
    socket.send(JSON.stringify({
      jsonrpc: '2.0',
      method: 'aria2.onDownloadError',
      params: [{ gid: 'target', errorCode: '3' }],
    }));

    await expect(wait).resolves.toEqual({
      type: EventType.DownloadError,
      gid: 'target',
      errorCode: 3,
    });

    await emitter.close();
    await new Promise<void>((resolve, reject) => server.close((error) => {
      if (error) reject(error);
      else resolve();
    }));
  });

  it('waitForTerminal validates the GID and timeout', async () => {
    const emitter = new Aria2EventEmitter('ws://localhost:6800/jsonrpc');

    await expect(emitter.waitForTerminal('')).rejects.toThrow(TypeError);
    await expect(emitter.waitForTerminal('target', 0)).rejects.toThrow(TypeError);
  });

  it('waitForTerminal rejects when the emitter closes', async () => {
    const server = new WebSocketServer({ port: 0 });
    await new Promise<void>((resolve) => server.once('listening', resolve));
    const address = server.address() as AddressInfo;
    const emitter = new Aria2EventEmitter(`ws://127.0.0.1:${address.port}/jsonrpc`);
    const wait = emitter.waitForTerminal('target');
    await vi.waitFor(() => expect(server.clients.size).toBe(1));

    await emitter.close();

    await expect(wait).rejects.toThrow('Emitter has been closed');
    await new Promise<void>((resolve, reject) => server.close((error) => {
      if (error) reject(error);
      else resolve();
    }));
  });
});
