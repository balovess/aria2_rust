import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import net from 'node:net';
import { HttpTransport, WebSocketTransport } from '../../src/transport.js';
import { RpcError, ConnectionError, TimeoutError } from '../../src/errors.js';

describe('HttpTransport', () => {
  let transport: HttpTransport;
  let mockFetch: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    mockFetch = vi.fn();
    vi.stubGlobal('fetch', mockFetch);
    transport = new HttpTransport('http://localhost:6800/jsonrpc');
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('builds correct JSON-RPC 2.0 request', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ jsonrpc: '2.0', id: 1, result: 'ok' }),
    });

    await transport.sendRequest('aria2.getVersion', []);

    expect(mockFetch).toHaveBeenCalledOnce();
    const call = mockFetch.mock.calls[0];
    const body = JSON.parse(call[1].body);
    expect(body.jsonrpc).toBe('2.0');
    expect(body.method).toBe('aria2.getVersion');
    expect(body.params).toEqual([]);
    expect(body.id).toBeTypeOf('number');
    expect(call[1].method).toBe('POST');
    expect(call[1].headers['Content-Type']).toBe('application/json');
  });

  it('auto-increments request ID', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ jsonrpc: '2.0', id: 1, result: 'ok' }),
    });

    await transport.sendRequest('aria2.getVersion', []);
    await transport.sendRequest('aria2.getVersion', []);

    const body1 = JSON.parse(mockFetch.mock.calls[0][1].body);
    const body2 = JSON.parse(mockFetch.mock.calls[1][1].body);
    expect(body2.id).toBe(body1.id + 1);
  });

  it('prepends token as "token:xxx" when configured', async () => {
    transport = new HttpTransport('http://localhost:6800/jsonrpc', {
      token: 'mysecret',
    });
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ jsonrpc: '2.0', id: 1, result: 'ok' }),
    });

    await transport.sendRequest('aria2.getVersion', []);

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.params[0]).toBe('token:mysecret');
  });

  it('prepends secret as "token:xxx" when configured', async () => {
    transport = new HttpTransport('http://localhost:6800/jsonrpc', {
      secret: 'mysecret',
    });
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ jsonrpc: '2.0', id: 1, result: 'ok' }),
    });

    await transport.sendRequest('aria2.getVersion', []);

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.params[0]).toBe('token:mysecret');
  });

  it('does not prepend token when not configured', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({ jsonrpc: '2.0', id: 1, result: 'ok' }),
    });

    await transport.sendRequest('aria2.getVersion', ['param1']);

    const body = JSON.parse(mockFetch.mock.calls[0][1].body);
    expect(body.params[0]).toBe('param1');
  });

  it('returns result on success', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve({ jsonrpc: '2.0', id: 1, result: { version: '0.3.2' } }),
    });

    const result = await transport.sendRequest('aria2.getVersion', []);
    expect(result).toEqual({ version: '0.3.2' });
  });

  it('throws RpcError on error response', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve({
          jsonrpc: '2.0',
          id: 1,
          error: { code: 1, message: 'Unauthorized' },
        }),
    });

    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow(RpcError);
    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow('Unauthorized');
  });

  it('throws RpcError with correct code on error response', async () => {
    mockFetch.mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve({
          jsonrpc: '2.0',
          id: 1,
          error: { code: 2, message: 'Not found' },
        }),
    });

    try {
      await transport.sendRequest('aria2.getVersion', []);
    } catch (err) {
      expect(err).toBeInstanceOf(RpcError);
      expect((err as RpcError).code).toBe(2);
    }
  });

  it('throws ConnectionError on non-ok HTTP response', async () => {
    mockFetch.mockResolvedValue({
      ok: false,
      status: 500,
      statusText: 'Internal Server Error',
    });

    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow(
      ConnectionError,
    );
    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow(
      'HTTP 500',
    );
  });

  it('throws ConnectionError on network failure', async () => {
    mockFetch.mockRejectedValue(new TypeError('fetch failed'));

    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow(
      ConnectionError,
    );
    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow(
      'fetch failed',
    );
  });

  it('throws TimeoutError on abort/timeout', async () => {
    transport = new HttpTransport('http://localhost:6800/jsonrpc', {
      timeout: 1,
    });

    const abortErr = new DOMException('The operation was aborted', 'AbortError');
    mockFetch.mockRejectedValue(abortErr);

    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow(
      TimeoutError,
    );
  });

  it('close resolves without error', async () => {
    await expect(transport.close()).resolves.toBeUndefined();
  });
});

describe('WebSocketTransport', () => {
  it('close rejects an in-flight connection attempt', async () => {
    const server = net.createServer();
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    const address = server.address() as net.AddressInfo;
    const transport = new WebSocketTransport(`ws://127.0.0.1:${address.port}`);
    const request = transport.sendRequest('aria2.getVersion', []);
    await new Promise<void>((resolve) => server.once('connection', () => resolve()));

    await transport.close();
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const boundedRequest = Promise.race([
      request,
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => reject(new Error('connection close timed out')), 500);
      }),
    ]);
    await expect(boundedRequest).rejects.toThrow('Transport closed');
    if (timeout) clearTimeout(timeout);
    await new Promise<void>((resolve, reject) => server.close((error) => {
      if (error) reject(error);
      else resolve();
    }));
  });

  it('cleans up when the socket fails before sending', async () => {
    const transport = new WebSocketTransport('ws://localhost:6800/jsonrpc');
    const internals = transport as unknown as {
      ws: {
        readyState: number;
        send: () => never;
        removeAllListeners: () => void;
        close: () => void;
      } | null;
      pending: Map<number, unknown>;
    };
    internals.ws = {
      readyState: 1,
      send: () => {
        throw new Error('socket closed');
      },
      removeAllListeners: () => {},
      close: () => {},
    };

    await expect(transport.sendRequest('aria2.getVersion', [])).rejects.toThrow(
      'socket closed',
    );
    expect(internals.pending.size).toBe(0);

    await transport.close();
  });
});
