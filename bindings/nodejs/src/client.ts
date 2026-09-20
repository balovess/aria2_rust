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

function parseObjectResult<T extends object = Record<string, unknown>>(
  result: unknown,
  method: string,
): T {
  if (result === null || typeof result !== 'object' || Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  return result as T;
}

function parseObjectListResult<T>(result: unknown, method: string): T[] {
  if (!Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  for (const [index, item] of result.entries()) {
    if (item === null || typeof item !== 'object' || Array.isArray(item)) {
      throw new Aria2Error(`Unexpected item type for ${method} at index ${index}`);
    }
  }
  return result as T[];
}

function parseArrayResult(result: unknown, method: string): unknown[] {
  if (!Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  return result;
}

function parseStringListResult(
  result: unknown,
  method: string,
  expectedLength?: number,
): string[] {
  if (!Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  if (expectedLength !== undefined && result.length !== expectedLength) {
    throw new Aria2Error(
      `Unexpected result length for ${method}: expected ${expectedLength}, got ${result.length}`,
    );
  }
  for (const [index, item] of result.entries()) {
    if (typeof item !== 'string') {
      throw new Aria2Error(`Unexpected item type for ${method} at index ${index}`);
    }
  }
  return result;
}

function parseChangeUriCounts(result: unknown): string[] {
  if (!Array.isArray(result)) {
    throw new Aria2Error('Unexpected result type for changeUri');
  }
  if (result.length !== 2) {
    throw new Aria2Error(
      `Unexpected result length for changeUri: expected 2, got ${result.length}`,
    );
  }
  return result.map((item, index) => {
    if (typeof item === 'string' && /^(0|[1-9]\d*)$/.test(item)) return item;
    if (typeof item === 'number' && Number.isSafeInteger(item) && item >= 0) {
      return String(item);
    }
    throw new Aria2Error(`Unexpected item type for changeUri at index ${index}`);
  });
}

function parseStringResult(result: unknown, method: string): string {
  if (typeof result !== 'string') {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  return result;
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
    const result = await this.transport.sendRequest('aria2.addUri', params);
    return parseStringResult(result, 'addUri');
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
    const result = await this.transport.sendRequest('aria2.addTorrent', params);
    return parseStringResult(result, 'addTorrent');
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
    const result = await this.transport.sendRequest('aria2.addMetalink', params);
    return parseStringListResult(result, 'addMetalink');
  }

  async remove(gid: string): Promise<string> {
    const result = await this.transport.sendRequest('aria2.remove', [gid]);
    return parseStringResult(result, 'remove');
  }

  async pause(gid: string): Promise<string> {
    const result = await this.transport.sendRequest('aria2.pause', [gid]);
    return parseStringResult(result, 'pause');
  }

  async unpause(gid: string): Promise<string> {
    const result = await this.transport.sendRequest('aria2.unpause', [gid]);
    return parseStringResult(result, 'unpause');
  }

  async forcePause(gid: string): Promise<string> {
    const result = await this.transport.sendRequest('aria2.forcePause', [gid]);
    return parseStringResult(result, 'forcePause');
  }

  async forceRemove(gid: string): Promise<string> {
    const result = await this.transport.sendRequest('aria2.forceRemove', [gid]);
    return parseStringResult(result, 'forceRemove');
  }

  async pauseAll(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.pauseAll', []);
    return parseStringResult(result, 'pauseAll');
  }

  async forcePauseAll(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.forcePauseAll', []);
    return parseStringResult(result, 'forcePauseAll');
  }

  async unpauseAll(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.unpauseAll', []);
    return parseStringResult(result, 'unpauseAll');
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
    const result = await this.transport.sendRequest('aria2.changeUri', params);
    return parseChangeUriCounts(result);
  }

  async tellStatus(gid: string, keys?: string[]): Promise<StatusInfo> {
    const params: unknown[] = [gid];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest('aria2.tellStatus', params);
    return parseObjectResult<StatusInfo>(result, 'tellStatus');
  }

  async getFiles(gid: string): Promise<FileInfo[]> {
    const result = await this.transport.sendRequest('aria2.getFiles', [gid]);
    return parseObjectListResult<FileInfo>(result, 'getFiles');
  }

  async getUris(gid: string): Promise<UriEntry[]> {
    const result = await this.transport.sendRequest('aria2.getUris', [gid]);
    return parseObjectListResult<UriEntry>(result, 'getUris');
  }

  async getServers(gid: string): Promise<ServerInfoIndex[]> {
    const result = await this.transport.sendRequest('aria2.getServers', [gid]);
    return parseObjectListResult<ServerInfoIndex>(result, 'getServers');
  }

  async getPeers(gid: string): Promise<PeerInfo[]> {
    const result = await this.transport.sendRequest('aria2.getPeers', [gid]);
    return parseObjectListResult<PeerInfo>(result, 'getPeers');
  }

  async getTrackers(gid: string): Promise<TrackerInfo[]> {
    const result = await this.transport.sendRequest('aria2.getTrackers', [gid]);
    return parseObjectListResult<TrackerInfo>(result, 'getTrackers');
  }

  async getDhtStatus(): Promise<DhtStatus> {
    const result = await this.transport.sendRequest('aria2.getDhtStatus', []);
    return parseObjectResult<DhtStatus>(result, 'getDhtStatus');
  }

  async tellActive(keys?: string[]): Promise<StatusInfo[]> {
    const params: unknown[] = [];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest('aria2.tellActive', params);
    return parseObjectListResult<StatusInfo>(result, 'tellActive');
  }

  async tellWaiting(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]> {
    const params: unknown[] = [offset, num];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest('aria2.tellWaiting', params);
    return parseObjectListResult<StatusInfo>(result, 'tellWaiting');
  }

  async tellStopped(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]> {
    const params: unknown[] = [offset, num];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest('aria2.tellStopped', params);
    return parseObjectListResult<StatusInfo>(result, 'tellStopped');
  }

  async getGlobalStat(): Promise<GlobalStat> {
    const result = await this.transport.sendRequest('aria2.getGlobalStat', []);
    return parseObjectResult<GlobalStat>(result, 'getGlobalStat');
  }

  async purgeDownloadResult(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.purgeDownloadResult', []);
    return parseStringResult(result, 'purgeDownloadResult');
  }

  async removeDownloadResult(gid: string): Promise<string> {
    const result = await this.transport.sendRequest('aria2.removeDownloadResult', [gid]);
    return parseStringResult(result, 'removeDownloadResult');
  }

  async getGlobalOption(): Promise<Record<string, unknown>> {
    const result = await this.transport.sendRequest('aria2.getGlobalOption', []);
    return parseObjectResult(result, 'getGlobalOption');
  }

  async changeGlobalOption(options: Record<string, unknown>): Promise<string> {
    const result = await this.transport.sendRequest('aria2.changeGlobalOption', [options]);
    return parseStringResult(result, 'changeGlobalOption');
  }

  async getOption(gid: string): Promise<Record<string, unknown>> {
    const result = await this.transport.sendRequest('aria2.getOption', [gid]);
    return parseObjectResult(result, 'getOption');
  }

  async changeOption(gid: string, options: Record<string, unknown>): Promise<string> {
    const result = await this.transport.sendRequest('aria2.changeOption', [gid, options]);
    return parseStringResult(result, 'changeOption');
  }

  async getVersion(): Promise<VersionInfo> {
    const result = await this.transport.sendRequest('aria2.getVersion', []);
    return parseObjectResult<VersionInfo>(result, 'getVersion');
  }

  async getSessionInfo(): Promise<SessionInfo> {
    const result = await this.transport.sendRequest('aria2.getSessionInfo', []);
    return parseObjectResult<SessionInfo>(result, 'getSessionInfo');
  }

  async shutdown(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.shutdown', []);
    return parseStringResult(result, 'shutdown');
  }

  async forceShutdown(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.forceShutdown', []);
    return parseStringResult(result, 'forceShutdown');
  }

  async saveSession(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.saveSession', []);
    return parseStringResult(result, 'saveSession');
  }

  async updateBrowserContext(context: unknown): Promise<string> {
    const result = await this.transport.sendRequest('aria2.updateBrowserContext', [context]);
    return parseStringResult(result, 'updateBrowserContext');
  }

  async clearBrowserContext(): Promise<string> {
    const result = await this.transport.sendRequest('aria2.clearBrowserContext', []);
    return parseStringResult(result, 'clearBrowserContext');
  }

  async systemMulticall(
    calls: Array<{ methodName: string; params?: unknown[] }>,
  ): Promise<unknown[]> {
    const result = await this.transport.sendRequest('system.multicall', [calls]);
    return parseArrayResult(result, 'system.multicall');
  }

  async systemListMethods(): Promise<string[]> {
    const result = await this.transport.sendRequest('system.listMethods', []);
    return parseStringListResult(result, 'system.listMethods');
  }

  async systemListNotifications(): Promise<string[]> {
    const result = await this.transport.sendRequest('system.listNotifications', []);
    return parseStringListResult(result, 'system.listNotifications');
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
