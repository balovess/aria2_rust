import type { Transport } from './transport.js';
import { HttpTransport, WebSocketTransport } from './transport.js';
import { Aria2EventEmitter } from './events.js';
import type {
  StatusInfo,
  GlobalStat,
  VersionInfo,
  SessionInfo,
  ClientOptions,
} from './types.js';
import { ConnectionError } from './errors.js';

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
    if (this.eventEmitter) {
      return this.eventEmitter;
    }

    const wsUrl = httpToWs(this.url);
    this.eventEmitter = new Aria2EventEmitter(wsUrl, this.options);
    await this.eventEmitter.connect();
    return this.eventEmitter;
  }

  async addUri(uris: string[], options?: Record<string, unknown>): Promise<string> {
    const params: unknown[] = [uris];
    if (options) params.push(options);
    return (await this.transport.sendRequest('aria2.addUri', params)) as string;
  }

  async addTorrent(torrent: Buffer, options?: Record<string, unknown>): Promise<string> {
    const params: unknown[] = [torrent.toString('base64')];
    if (options) params.push(options);
    return (await this.transport.sendRequest('aria2.addTorrent', params)) as string;
  }

  async addMetalink(metalink: Buffer, options?: Record<string, unknown>): Promise<string> {
    const params: unknown[] = [metalink.toString('base64')];
    if (options) params.push(options);
    return (await this.transport.sendRequest('aria2.addMetalink', params)) as string;
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

  async forceUnpause(gid: string): Promise<string> {
    return (await this.transport.sendRequest('aria2.forceUnpause', [gid])) as string;
  }

  async tellStatus(gid: string, keys?: string[]): Promise<StatusInfo> {
    const params: unknown[] = [gid];
    if (keys) params.push(keys);
    return (await this.transport.sendRequest('aria2.tellStatus', params)) as StatusInfo;
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
    return (await this.transport.sendRequest('aria2.getGlobalOption', [])) as Record<string, unknown>;
  }

  async changeGlobalOption(options: Record<string, unknown>): Promise<string> {
    return (await this.transport.sendRequest('aria2.changeGlobalOption', [options])) as string;
  }

  async getOption(gid: string): Promise<Record<string, unknown>> {
    return (await this.transport.sendRequest('aria2.getOption', [gid])) as Record<string, unknown>;
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

  on(event: WsEventName | 'reconnecting' | 'close', handler: (...args: unknown[]) => void): this {
    this.ensureEventEmitter()
      .then((emitter) => {
        emitter.on(event, handler);
      })
      .catch(() => {
        throw new ConnectionError(`Failed to connect event emitter for event: ${event}`);
      });
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
