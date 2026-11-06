import WebSocket from 'ws';
import type { ClientOptions } from './types.js';
import { RpcError, ConnectionError, TimeoutError, AuthError } from './errors.js';

export interface Transport {
  sendRequest(method: string, params: unknown[]): Promise<unknown>;
  close(): Promise<void>;
}

export type EventCallback = (method: string, params: unknown[]) => void;

interface JsonRpcRequest {
  jsonrpc: '2.0';
  id: number;
  method: string;
  params: unknown[];
}

interface JsonRpcResponse {
  jsonrpc: '2.0';
  id: number;
  result?: unknown;
  error?: { code: number; message: string };
}

interface JsonRpcNotification {
  jsonrpc: '2.0';
  method: string;
  params: unknown[];
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface PendingHttpRequest {
  controller: AbortController;
  closed: boolean;
}

function isAuthRpcError(code: unknown, message: string): boolean {
  if (code === -32001) return true;

  const normalized = message.toLowerCase();
  return [
    'unauthorized',
    'auth fail',
    'authentication',
    'authorization',
    'invalid token',
    'token required',
  ].some((marker) => normalized.includes(marker));
}

function buildParams(token: string | undefined, params: unknown[]): unknown[] {
  const result: unknown[] = [];
  if (token) {
    result.push(`token:${token}`);
  }
  result.push(...params);
  return result;
}

export class HttpTransport implements Transport {
  private url: string;
  private token: string | undefined;
  private timeout: number;
  private nextId = 1;
  private closed = false;
  private pending = new Map<number, PendingHttpRequest>();

  constructor(url: string, options?: ClientOptions) {
    this.url = url;
    this.token = options?.token ?? options?.secret;
    this.timeout = options?.timeout ?? 30_000;
  }

  async sendRequest(method: string, params: unknown[]): Promise<unknown> {
    if (this.closed) {
      throw new ConnectionError('Transport closed');
    }

    const id = this.nextId++;
    const request: JsonRpcRequest = {
      jsonrpc: '2.0',
      id,
      method,
      params: buildParams(this.token, params),
    };

    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);
    const pendingRequest: PendingHttpRequest = { controller, closed: false };
    this.pending.set(id, pendingRequest);

    try {
      const response = await fetch(this.url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request),
        signal: controller.signal,
      });

      // Try to parse response body even for non-200 status
      // aria2 may return HTTP 400 with JSON-RPC error in body
      let data: JsonRpcResponse;
      try {
        data = (await response.json()) as JsonRpcResponse;
      } catch {
        // If we can't parse JSON, throw ConnectionError for non-200 status
        if (!response.ok) {
          throw new ConnectionError(`HTTP ${response.status}: ${response.statusText}`);
        }
        throw new ConnectionError('Invalid JSON response');
      }

      if (data.error) {
        if (isAuthRpcError(data.error.code, data.error.message)) {
          throw new AuthError(data.error.message);
        }
        throw new RpcError(data.error.message, data.error.code);
      }

      if (!response.ok) {
        throw new ConnectionError(`HTTP ${response.status}: ${response.statusText}`);
      }

      return data.result;
    } catch (err: unknown) {
      if (err instanceof RpcError || err instanceof AuthError || err instanceof ConnectionError) {
        throw err;
      }
      if (pendingRequest.closed) {
        throw new ConnectionError('Transport closed');
      }
      if (err instanceof DOMException && err.name === 'AbortError') {
        throw new TimeoutError(`Request timed out after ${this.timeout}ms`);
      }
      if (err instanceof Error && err.name === 'AbortError') {
        throw new TimeoutError(`Request timed out after ${this.timeout}ms`);
      }
      throw new ConnectionError(err instanceof Error ? err.message : String(err));
    } finally {
      clearTimeout(timer);
      this.pending.delete(id);
    }
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    for (const pendingRequest of this.pending.values()) {
      pendingRequest.closed = true;
      pendingRequest.controller.abort();
    }
  }
}

export class WebSocketTransport implements Transport {
  private url: string;
  private token: string | undefined;
  private timeout: number;
  private nextId = 1;
  private ws: WebSocket | null = null;
  private pending = new Map<number, PendingRequest>();
  private onEvent: EventCallback | null = null;
  private connectPromise: Promise<void> | null = null;
  private pendingWs: WebSocket | null = null;
  private pendingConnectReject: ((reason: Error) => void) | null = null;
  private closed = false;

  constructor(url: string, options?: ClientOptions, onEvent?: EventCallback) {
    this.url = url;
    this.token = options?.token ?? options?.secret;
    this.timeout = options?.timeout ?? 30_000;
    this.onEvent = onEvent ?? null;
  }

  setEventHandler(handler: EventCallback): void {
    this.onEvent = handler;
  }

  private async ensureConnection(): Promise<void> {
    if (this.closed) {
      throw new ConnectionError('Transport closed');
    }

    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      return;
    }

    if (this.connectPromise) {
      await this.connectPromise;
      return;
    }

    this.connectPromise = new Promise<void>((resolve, reject) => {
      const ws = new WebSocket(this.url);
      let settled = false;

      const cleanup = (): void => {
        ws.removeListener('open', openHandler);
        ws.removeListener('error', errorHandler);
        ws.removeListener('close', closeHandler);
        if (this.pendingWs === ws) this.pendingWs = null;
        if (this.pendingConnectReject === rejectConnection) {
          this.pendingConnectReject = null;
        }
      };

      const rejectConnection = (error: Error): void => {
        if (settled) return;
        settled = true;
        cleanup();
        this.connectPromise = null;
        reject(error);
      };

      const openHandler = (): void => {
        if (settled) return;
        settled = true;
        cleanup();
        this.ws = ws;
        this.connectPromise = null;
        resolve();
      };

      const errorHandler = (err: Error): void => {
        rejectConnection(new ConnectionError(err.message));
      };

      const closeHandler = (): void => {
        rejectConnection(new ConnectionError('WebSocket connection closed'));
        this.rejectAllPending(new ConnectionError('WebSocket connection closed'));
      };

      this.pendingWs = ws;
      this.pendingConnectReject = rejectConnection;
      ws.once('open', openHandler);
      ws.once('error', errorHandler);
      ws.once('close', closeHandler);
      ws.on('message', (data: WebSocket.Data) => {
        this.handleMessage(data);
      });
    });

    await this.connectPromise;
  }

  private handleMessage(data: WebSocket.Data): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(String(data));
    } catch {
      return;
    }

    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return;
    }
    const obj = parsed as Record<string, unknown>;

    if (typeof obj.method === 'string' && !('id' in obj)) {
      const notification = obj as unknown as JsonRpcNotification;
      if (this.onEvent) {
        this.onEvent(notification.method, Array.isArray(notification.params) ? notification.params : []);
      }
      return;
    }

    if ('id' in obj) {
      const response = obj as unknown as JsonRpcResponse;
      const pending = this.pending.get(response.id);
      if (!pending) return;

      clearTimeout(pending.timer);
      this.pending.delete(response.id);

      if (response.error && typeof response.error === 'object') {
        pending.reject(new RpcError(
          typeof response.error.message === 'string'
            ? response.error.message
            : 'Unknown RPC error',
          typeof response.error.code === 'number' ? response.error.code : -1,
        ));
      } else if ('error' in response) {
        pending.reject(new RpcError('Malformed RPC error response', -1));
      } else {
        pending.resolve(response.result);
      }
    }
  }

  private rejectAllPending(error: Error): void {
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(error);
      this.pending.delete(id);
    }
  }

  async sendRequest(method: string, params: unknown[]): Promise<unknown> {
    await this.ensureConnection();

    const id = this.nextId++;
    const request: JsonRpcRequest = {
      jsonrpc: '2.0',
      id,
      method,
      params: buildParams(this.token, params),
    };

    return new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new TimeoutError(`Request timed out after ${this.timeout}ms`));
      }, this.timeout);

      this.pending.set(id, { resolve, reject, timer });
      const ws = this.ws;
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(new ConnectionError('WebSocket is not connected'));
        return;
      }

      try {
        ws.send(JSON.stringify(request), (err?: Error) => {
          if (err) {
            clearTimeout(timer);
            this.pending.delete(id);
            reject(new ConnectionError(err.message));
          }
        });
      } catch (err: unknown) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(new ConnectionError(err instanceof Error ? err.message : String(err)));
      }
    });
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;

    const pendingReject = this.pendingConnectReject;
    this.pendingConnectReject = null;
    if (this.pendingWs) {
      const pendingWs = this.pendingWs;
      pendingWs.removeAllListeners();
      pendingWs.once('error', () => {});
      pendingWs.terminate();
      this.pendingWs = null;
    }
    this.connectPromise = null;
    pendingReject?.(new ConnectionError('Transport closed'));

    if (this.ws) {
      this.ws.removeAllListeners();
      this.ws.close();
      this.ws = null;
    }
    this.rejectAllPending(new ConnectionError('Transport closed'));
  }
}
