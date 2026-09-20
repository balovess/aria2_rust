import { EventEmitter } from 'events';
import WebSocket from 'ws';
import type { ClientOptions, DownloadEvent, FileInfo } from './types.js';
import { EventType } from './types.js';
import { ConnectionError, TimeoutError } from './errors.js';

const EVENT_MAP: Record<string, string> = {
  [EventType.DownloadStart]: 'downloadStart',
  [EventType.DownloadPause]: 'downloadPause',
  [EventType.DownloadStop]: 'downloadStop',
  [EventType.DownloadComplete]: 'downloadComplete',
  [EventType.DownloadError]: 'downloadError',
  [EventType.BtDownloadComplete]: 'btDownloadComplete',
};

const TERMINAL_EVENT_NAMES = [
  'downloadStop',
  'downloadComplete',
  'downloadError',
  'btDownloadComplete',
] as const;

const MAX_RECONNECT_RETRIES = 5;
const BASE_RECONNECT_DELAY = 1000;

export class Aria2EventEmitter extends EventEmitter {
  private wsUrl: string;
  private ws: WebSocket | null = null;
  private pendingWs: WebSocket | null = null;
  private reconnectAttempts = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closed = false;
  private connectPromise: Promise<void> | null = null;
  private pendingConnectReject: ((error: Error) => void) | null = null;
  private terminalWaiters = new Set<(error: Error) => void>();

  constructor(wsUrl: string, _options?: ClientOptions) {
    super();
    this.wsUrl = wsUrl;
  }

  async connect(): Promise<void> {
    if (this.closed) {
      throw new ConnectionError('Emitter has been closed');
    }

    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      return;
    }

    if (this.connectPromise) {
      await this.connectPromise;
      return;
    }

    this.connectPromise = this.doConnect();

    try {
      await this.connectPromise;
    } finally {
      this.connectPromise = null;
    }
  }

  /**
   * Wait for a terminal event for one GID without polling status.
   *
   * Events for other GIDs and non-terminal transitions are ignored. Register
   * the wait before submitting a task when a fast completion must not be
   * missed. The timeout covers waiting for the event after the WebSocket is
   * connected; omit it for an unbounded download.
   */
  async waitForTerminal(gid: string, timeoutMs?: number): Promise<DownloadEvent> {
    if (typeof gid !== 'string' || gid.length === 0) {
      throw new TypeError('gid must be a non-empty string');
    }
    if (
      timeoutMs !== undefined &&
      (!Number.isFinite(timeoutMs) || timeoutMs <= 0)
    ) {
      throw new TypeError('timeoutMs must be a positive number or undefined');
    }

    let timer: ReturnType<typeof setTimeout> | null = null;
    let settled = false;
    let cleanup = (): void => {};
    let startTimer = (): void => {};
    let rejectClosed: (error: Error) => void = () => {};
    const result = new Promise<DownloadEvent>((resolve, reject) => {
      const onEvent = (event: DownloadEvent): void => {
        if (event.gid !== gid) return;
        settled = true;
        cleanup();
        resolve(event);
      };

      cleanup = (): void => {
        for (const eventName of TERMINAL_EVENT_NAMES) {
          this.off(eventName, onEvent);
        }
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
        this.terminalWaiters.delete(rejectClosed);
      };

      for (const eventName of TERMINAL_EVENT_NAMES) {
        this.on(eventName, onEvent);
      }

      startTimer = (): void => {
        if (timeoutMs === undefined || settled) return;
        timer = setTimeout(() => {
          if (settled) return;
          settled = true;
          cleanup();
          reject(new TimeoutError(`Timed out waiting for terminal event for GID ${gid}`));
        }, timeoutMs);
      };

      rejectClosed = (error: Error): void => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(error);
      };
      this.terminalWaiters.add(rejectClosed);
    });

    try {
      await this.connect();
      startTimer();
      return await result;
    } catch (error) {
      cleanup();
      throw error;
    }
  }

  private async doConnect(): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      const ws = new WebSocket(this.wsUrl);
      this.pendingWs = ws;

      let settled = false;
      const cleanup = (): void => {
        ws.removeListener('open', openHandler);
        ws.removeListener('error', errorHandler);
        ws.removeListener('close', closeHandler);
        if (this.pendingWs === ws) {
          this.pendingWs = null;
        }
        if (this.pendingConnectReject === rejectPendingConnection) {
          this.pendingConnectReject = null;
        }
      };

      const rejectConnection = (message: string): void => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(new ConnectionError(message));
      };

      const rejectPendingConnection = (error: Error): void => {
        rejectConnection(error.message);
      };

      const openHandler = (): void => {
        if (this.closed) {
          rejectConnection('Emitter has been closed');
          ws.close();
          return;
        }

        settled = true;
        cleanup();
        this.ws = ws;
        this.reconnectAttempts = 0;
        this.setupMessageHandler(ws);
        this.setupCloseHandler(ws);
        resolve();
      };

      const errorHandler = (err: Error): void => {
        rejectConnection(this.closed ? 'Emitter has been closed' : err.message);
      };

      const closeHandler = (code: number): void => {
        rejectConnection(
          this.closed
            ? 'Emitter has been closed'
            : `WebSocket closed before connection established (code ${code})`,
        );
      };

      this.pendingConnectReject = rejectPendingConnection;
      ws.once('open', openHandler);
      ws.once('error', errorHandler);
      ws.once('close', closeHandler);
    });
  }

  private setupMessageHandler(ws: WebSocket): void {
    ws.on('message', (data: WebSocket.Data) => {
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
      if (typeof obj.method !== 'string') return;

      const method = obj.method;
      const eventName = EVENT_MAP[method];
      if (!eventName) return;

      const params = Array.isArray(obj.params) ? obj.params : [];
      const details = params[0];
      const detailsObject =
        details !== null && typeof details === 'object'
          ? (details as Record<string, unknown>)
          : undefined;
      const gid = typeof detailsObject?.gid === 'string' ? detailsObject.gid : String(details);

      const event: DownloadEvent = {
        type: method as EventType,
        gid,
      };

      const errorCode = detailsObject?.errorCode;
      if (typeof errorCode === 'number' && Number.isSafeInteger(errorCode)) {
        event.errorCode = errorCode;
      } else if (typeof errorCode === 'string' && /^-?\d+$/.test(errorCode)) {
        const parsed = Number(errorCode);
        if (Number.isSafeInteger(parsed)) event.errorCode = parsed;
      }

      if (Array.isArray(detailsObject?.files)) {
        event.files = detailsObject.files as FileInfo[];
      }

      this.emit(eventName, event);
    });
  }

  private setupCloseHandler(ws: WebSocket): void {
    ws.once('close', (code: number, reason: Buffer) => {
      if (this.ws === ws) {
        this.ws = null;
      }

      if (!this.closed) {
        this.emit('close', code, reason.toString());
        this.attemptReconnect();
      }
    });

    ws.once('error', () => {
      if (this.ws === ws) {
        this.ws = null;
      }
    });
  }

  private attemptReconnect(): void {
    if (this.closed) return;
    if (this.reconnectAttempts >= MAX_RECONNECT_RETRIES) {
      this.emit('reconnecting', false, this.reconnectAttempts);
      return;
    }

    const delay = BASE_RECONNECT_DELAY * Math.pow(2, this.reconnectAttempts);
    this.reconnectAttempts++;
    this.emit('reconnecting', true, this.reconnectAttempts);

    this.reconnectTimer = setTimeout(async () => {
      if (this.closed) return;
      try {
        await this.connect();
      } catch {
        this.attemptReconnect();
      }
    }, delay);
  }

  async close(): Promise<void> {
    this.closed = true;

    for (const reject of this.terminalWaiters) {
      reject(new ConnectionError('Emitter has been closed'));
    }
    this.terminalWaiters.clear();

    const pendingWs = this.pendingWs;
    const pendingReject = this.pendingConnectReject;
    this.pendingConnectReject = null;
    pendingReject?.(new ConnectionError('Emitter has been closed'));

    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }

    if (this.ws) {
      this.ws.removeAllListeners();
      this.ws.close();
      this.ws = null;
    }

    if (pendingWs) {
      pendingWs.removeAllListeners();
      pendingWs.once('error', () => {});
      pendingWs.terminate();
      if (this.pendingWs === pendingWs) {
        this.pendingWs = null;
      }
    }

    this.connectPromise = null;
    this.removeAllListeners();
  }
}
