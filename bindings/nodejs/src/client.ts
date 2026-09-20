import type { Transport } from './transport.js';
import { HttpTransport, WebSocketTransport } from './transport.js';
import { Aria2EventEmitter } from './events.js';
import type {
  StatusInfo,
  FileInfo,
  GlobalStat,
  VersionInfo,
  SessionInfo,
  UriEntry,
  ServerInfoIndex,
  PeerInfo,
  TrackerInfo,
  DhtStatus,
  ClientOptions,
} from './types.js';
import { Aria2Error } from './errors.js';
import type { PositionMode } from './types.js';

const DEFAULT_URL = 'http://localhost:6800/jsonrpc';
const WS_EVENT_NAMES = [
  'downloadStart',
  'downloadPause',
  'downloadStop',
  'downloadComplete',
  'downloadError',
  'btDownloadComplete',
  'btDownloadError',
] as const;

type WsEventName = (typeof WS_EVENT_NAMES)[number];

function parseObjectResult(result: unknown, method: string): Record<string, unknown> {
  if (result === null || typeof result !== 'object' || Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  return result as Record<string, unknown>;
}

function httpToWs(url: string): string {
  if (url.startsWith('https://')) {
    return url.replace('https://', 'wss://');
  }
  if (url.startsWith('http://')) {
    return url.replace('http://', 'ws://');
  }
  return url;
}

export class Aria2Client {
  private transport: Transport;
  private eventEmitter: Aria2EventEmitter | null = null;
  private url: string;
  private options: ClientOptions | undefined;

  constructor(url?: string, options?: ClientOptions) {
    this.url = url ?? DEFAULT_URL;
    this.options = options;

    if (this.url.startsWith('ws://') || this.url.startsWith('wss://')) {
      this.transport = new WebSocketTransport(this.url, options);
    } else {
      this.transport = new HttpTransport(this.url, options);
    }
  }

  private async ensureEventEmitter(): Promise<Aria2EventEmitter> {
    const emitter = this.getOrCreateEventEmitter();
    await emitter.connect();
    return emitter;
  }

  private getOrCreateEventEmitter(): Aria2EventEmitter {
    if (this.eventEmitter) {
      return this.eventEmitter;
    }

    const wsUrl = httpToWs(this.url);
    this.eventEmitter = new Aria2EventEmitter(wsUrl, this.options);
    return this.eventEmitter;
  }

  /** Connect the notification WebSocket before starting a download. */
  async connectEvents(): Promise<Aria2EventEmitter> {
    return this.ensureEventEmitter();
  }

  async call<T = unknown>(method: string, params: unknown[] = []): Promise<T> {
    return (await this.transport.sendRequest(method, params)) as T;
  }

  async addUri(
    uris: string[],
    options?: Record<string, unknown>,
    position?: number,
  ): Promise<string> {
    const params: unknown[] = [uris];
    if (options !== undefined || position !== undefined) params.push(options ?? {});
    if (position !== undefined) params.push(position);
    return (await this.transport.sendRequest('aria2.addUri', params)) as string;
  }

  async addTorrent(
    torrent: Buffer,
    options?: Record<string, unknown>,
    webSeedUris?: string[],
    position?: number,
  ): Promise<string> {
    const params: unknown[] = [torrent.toString('base64')];
    if (webSeedUris !== undefined || options !== undefined || position !== undefined) {
      params.push(webSeedUris ?? []);
    }
    if (options !== undefined || position !== undefined) params.push(options ?? {});
    if (position !== undefined) params.push(position);
    return (await this.transport.sendRequest('aria2.addTorrent', params)) as string;
  }

  async addMetalink(
    metalink: Buffer,
    options?: Record<string, unknown>,
    position?: number,
  ): Promise<string[]> {
    const params: unknown[] = [metalink.toString('base64')];
    if (options !== undefined) params.push(options);
    else if (position !== undefined) params.push({});
    if (position !== undefined) params.push(position);
    return (await this.transport.sendRequest('aria2.addMetalink', params)) as string[];
  }

  async remove(gid: string): Promise<string> {
    return (await this.transport.sendRequest('aria2.remove', [gid])) as string;
  }

  async pause(gid: string): Promise<string> {
    return (await this.transport.sendRequest('aria2.pause', [gid])) as string;
  }

  async unpause(gid: string): Promise<string> {
    return (await this.transport.sendRequest('aria2.unpause', [gid])) as string;
  }

  async forcePause(gid: string): Promise<string> {
    return (await this.transport.sendRequest('aria2.forcePause', [gid])) as string;
  }

  async forceRemove(gid: string): Promise<string> {
    return (await this.transport.sendRequest('aria2.forceRemove', [gid])) as string;
  }

  async pauseAll(): Promise<string> {
    return (await this.transport.sendRequest('aria2.pauseAll', [])) as string;
  }

  async forcePauseAll(): Promise<string> {
    return (await this.transport.sendRequest('aria2.forcePauseAll', [])) as string;
  }

  async unpauseAll(): Promise<string> {
    return (await this.transport.sendRequest('aria2.unpauseAll', [])) as string;
  }

  async changePosition(gid: string, position: number, mode: PositionMode): Promise<number> {
    const result = await this.transport.sendRequest('aria2.changePosition', [gid, position, mode]);
    if (typeof result === 'number' && Number.isSafeInteger(result) && result >= 0) {
      return result;
    }
    if (typeof result === 'string' && /^(0|[1-9]\d*)$/.test(result)) {
      const parsed = Number(result);
      if (Number.isSafeInteger(parsed)) return parsed;
    }
    throw new Aria2Error(`Unexpected result type for changePosition: ${typeof result}`);
  }

  async changeUri(
    gid: string,
    fileIndex: number,
    deleteUris: string[],
    addUris: string[],
    position?: number,
  ): Promise<string[]> {
    const params: unknown[] = [gid, fileIndex, deleteUris, addUris];
    if (position !== undefined) params.push(position);
    return (await this.transport.sendRequest('aria2.changeUri', params)) as string[];
  }

  async tellStatus(gid: string, keys?: string[]): Promise<StatusInfo> {
    const params: unknown[] = [gid];
    if (keys) params.push(keys);
    return (await this.transport.sendRequest('aria2.tellStatus', params)) as StatusInfo;
  }

  async getFiles(gid: string): Promise<FileInfo[]> {
    return (await this.transport.sendRequest('aria2.getFiles', [gid])) as FileInfo[];
  }

  async getUris(gid: string): Promise<UriEntry[]> {
    return (await this.transport.sendRequest('aria2.getUris', [gid])) as UriEntry[];
  }

  async getServers(gid: string): Promise<ServerInfoIndex[]> {
    return (await this.transport.sendRequest('aria2.getServers', [gid])) as ServerInfoIndex[];
  }

  async getPeers(gid: string): Promise<PeerInfo[]> {
    return (await this.transport.sendRequest('aria2.getPeers', [gid])) as PeerInfo[];
  }

  async getTrackers(gid: string): Promise<TrackerInfo[]> {
    return (await this.transport.sendRequest('aria2.getTrackers', [gid])) as TrackerInfo[];
  }

  async getDhtStatus(): Promise<DhtStatus> {
    return (await this.transport.sendRequest('aria2.getDhtStatus', [])) as DhtStatus;
  }

  async tellActive(keys?: string[]): Promise<StatusInfo[]> {
    const params: unknown[] = [];
    if (keys) params.push(keys);
    return (await this.transport.sendRequest('aria2.tellActive', params)) as StatusInfo[];
  }

  async tellWaiting(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]> {
    const params: unknown[] = [offset, num];
    if (keys) params.push(keys);
    return (await this.transport.sendRequest('aria2.tellWaiting', params)) as StatusInfo[];
  }

  async tellStopped(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]> {
    const params: unknown[] = [offset, num];
    if (keys) params.push(keys);
    return (await this.transport.sendRequest('aria2.tellStopped', params)) as StatusInfo[];
  }

  async getGlobalStat(): Promise<GlobalStat> {
    return (await this.transport.sendRequest('aria2.getGlobalStat', [])) as GlobalStat;
  }

  async purgeDownloadResult(): Promise<string> {
    return (await this.transport.sendRequest('aria2.purgeDownloadResult', [])) as string;
  }

  async removeDownloadResult(gid: string): Promise<string> {
    return (await this.transport.sendRequest('aria2.removeDownloadResult', [gid])) as string;
  }

  async getGlobalOption(): Promise<Record<string, unknown>> {
    const result = await this.transport.sendRequest('aria2.getGlobalOption', []);
    return parseObjectResult(result, 'getGlobalOption');
  }

  async changeGlobalOption(options: Record<string, unknown>): Promise<string> {
    return (await this.transport.sendRequest('aria2.changeGlobalOption', [options])) as string;
  }

  async getOption(gid: string): Promise<Record<string, unknown>> {
    const result = await this.transport.sendRequest('aria2.getOption', [gid]);
    return parseObjectResult(result, 'getOption');
  }

  async changeOption(gid: string, options: Record<string, unknown>): Promise<string> {
    return (await this.transport.sendRequest('aria2.changeOption', [gid, options])) as string;
  }

  async getVersion(): Promise<VersionInfo> {
    return (await this.transport.sendRequest('aria2.getVersion', [])) as VersionInfo;
  }

  async getSessionInfo(): Promise<SessionInfo> {
    return (await this.transport.sendRequest('aria2.getSessionInfo', [])) as SessionInfo;
  }

  async shutdown(): Promise<string> {
    return (await this.transport.sendRequest('aria2.shutdown', [])) as string;
  }

  async forceShutdown(): Promise<string> {
    return (await this.transport.sendRequest('aria2.forceShutdown', [])) as string;
  }

  async saveSession(): Promise<string> {
    return (await this.transport.sendRequest('aria2.saveSession', [])) as string;
  }

  async updateBrowserContext(context: unknown): Promise<string> {
    return (await this.transport.sendRequest('aria2.updateBrowserContext', [context])) as string;
  }

  async clearBrowserContext(): Promise<string> {
    return (await this.transport.sendRequest('aria2.clearBrowserContext', [])) as string;
  }

  async systemMulticall(
    calls: Array<{ methodName: string; params?: unknown[] }>,
  ): Promise<unknown[]> {
    return (await this.transport.sendRequest('system.multicall', [calls])) as unknown[];
  }

  async systemListMethods(): Promise<string[]> {
    return (await this.transport.sendRequest('system.listMethods', [])) as string[];
  }

  async systemListNotifications(): Promise<string[]> {
    return (await this.transport.sendRequest('system.listNotifications', [])) as string[];
  }

  private registerEventListener(
    event: WsEventName | 'reconnecting' | 'close',
    handler: (...args: unknown[]) => void,
    once: boolean,
  ): this {
    const emitter = this.getOrCreateEventEmitter();
    if (once) emitter.once(event, handler);
    else emitter.on(event, handler);
    void this.ensureEventEmitter().catch(() => {
      // Listener registration is intentionally fire-and-forget for compatibility. Callers
      // that need connection errors can await `connectEvents()` instead.
    });
    return this;
  }

  on(event: WsEventName | 'reconnecting' | 'close', handler: (...args: unknown[]) => void): this {
    return this.registerEventListener(event, handler, false);
  }

  once(event: WsEventName | 'reconnecting' | 'close', handler: (...args: unknown[]) => void): this {
    return this.registerEventListener(event, handler, true);
  }

  off(event: WsEventName | 'reconnecting' | 'close', handler: (...args: unknown[]) => void): this {
    this.eventEmitter?.off(event, handler);
    return this;
  }

  async close(): Promise<void> {
    await this.transport.close();
    if (this.eventEmitter) {
      await this.eventEmitter.close();
      this.eventEmitter = null;
    }
  }

  destroy(): void {
    this.transport.close().catch(() => {});
    if (this.eventEmitter) {
      this.eventEmitter.close().catch(() => {});
      this.eventEmitter = null;
    }
  }
}
