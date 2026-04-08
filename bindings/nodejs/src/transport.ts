import WebSocket from 'ws';
import type { ClientOptions } from './types.js';
import { RpcError, ConnectionError, TimeoutError } from './errors.js';

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

  constructor(url: string, options?: ClientOptions) {
    this.url = url;
    this.token = options?.token ?? options?.secret;
    this.timeout = options?.timeout ?? 30_000;
  }

  async sendRequest(method: string, params: unknown[]): Promise<unknown> {
    const id = this.nextId++;
    const request: JsonRpcRequest = {
      jsonrpc: '2.0',
      id,
      method,
      params: buildParams(this.token, params),
    };

    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);

    try {
      const response = await fetch(this.url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request),
        signal: controller.signal,
      });

      if (!response.ok) {
        throw new ConnectionError(`HTTP ${response.status}: ${response.statusText}`);
      }

      const data = (await response.json()) as JsonRpcResponse;

      if (data.error) {
        if (data.error.code === 1) {
          throw new RpcError(data.error.message, data.error.code);
        }
        throw new RpcError(data.error.message, data.error.code);
      }

      return data.result;
    } catch (err: unknown) {
      if (err instanceof RpcError || err instanceof ConnectionError) {
        throw err;
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
    }
  }

  async close(): Promise<void> {}
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
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      return;
    }

    if (this.connectPromise) {
      await this.connectPromise;
      return;
    }

    this.connectPromise = new Promise<void>((resolve, reject) => {
      const ws = new WebSocket(this.url);

      ws.once('open', () => {
        this.ws = ws;
        this.connectPromise = null;
        resolve();
      });

      ws.once('error', (err: Error) => {
        this.ws = null;
        this.connectPromise = null;
        reject(new ConnectionError(err.message));
      });

      ws.once('close', () => {
        this.ws = null;
        this.connectPromise = null;
        this.rejectAllPending(new ConnectionError('WebSocket connection closed'));
      });

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

    const obj = parsed as Record<string, unknown>;

    if ('method' in obj && !('id' in obj)) {
      const notification = obj as unknown as JsonRpcNotification;
      if (this.onEvent) {
        this.onEvent(notification.method, notification.params);
      }
      return;
    }

    if ('id' in obj) {
      const response = obj as unknown as JsonRpcResponse;
      const pending = this.pending.get(response.id);
      if (!pending) return;

      clearTimeout(pending.timer);
      this.pending.delete(response.id);

      if (response.error) {
        pending.reject(new RpcError(response.error.message, response.error.code));
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

      this.ws!.send(JSON.stringify(request), (err?: Error) => {
        if (err) {
          clearTimeout(timer);
          this.pending.delete(id);
          reject(new ConnectionError(err.message));
        }
      });
    });
  }

  async close(): Promise<void> {
    if (this.ws) {
      this.ws.removeAllListeners();
      this.ws.close();
      this.ws = null;
    }
    this.rejectAllPending(new ConnectionError('Transport closed'));
    this.connectPromise = null;
  }
}
