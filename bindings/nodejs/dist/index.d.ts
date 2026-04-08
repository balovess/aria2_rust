import { EventEmitter } from 'events';

interface StatusInfo {
    gid: string;
    totalLength?: string;
    completedLength?: string;
    uploadLength?: string;
    downloadSpeed?: string;
    uploadSpeed?: string;
    errorCode?: string;
    status: DownloadStatus;
    dir?: string;
    files?: FileInfo[];
}
interface GlobalStat {
    downloadSpeed: string;
    uploadSpeed: string;
    numActive: string;
    numWaiting: string;
    numStopped: string;
    numStoppedTotal: string;
}
interface VersionInfo {
    version: string;
    enabledFeatures: string[];
}
interface SessionInfo {
    sessionId: string;
}
interface FileInfo {
    index: string;
    path: string;
    length: string;
    completedLength: string;
    selected: string;
    uris: UriEntry[];
}
interface UriEntry {
    uri: string;
    status: 'used' | 'waiting';
}
interface DownloadEvent {
    type: EventType;
    gid: string;
    errorCode?: number;
    files?: unknown[];
}
declare const enum EventType {
    DownloadStart = "aria2.onDownloadStart",
    DownloadPause = "aria2.onDownloadPause",
    DownloadStop = "aria2.onDownloadStop",
    DownloadComplete = "aria2.onDownloadComplete",
    DownloadError = "aria2.onDownloadError",
    BtDownloadComplete = "aria2.onBtDownloadComplete",
    BtDownloadError = "aria2.onBtDownloadError"
}
declare const enum DownloadStatus {
    Active = "active",
    Waiting = "waiting",
    Paused = "paused",
    Error = "error",
    Complete = "complete",
    Removed = "removed"
}
interface ClientOptions {
    token?: string;
    timeout?: number;
    secret?: string;
}

declare const WS_EVENT_NAMES: readonly ["downloadStart", "downloadPause", "downloadStop", "downloadComplete", "downloadError", "btDownloadComplete", "btDownloadError"];
type WsEventName = (typeof WS_EVENT_NAMES)[number];
declare class Aria2Client {
    private transport;
    private eventEmitter;
    private url;
    private options;
    constructor(url?: string, options?: ClientOptions);
    private ensureEventEmitter;
    addUri(uris: string[], options?: Record<string, unknown>): Promise<string>;
    addTorrent(torrent: Buffer, options?: Record<string, unknown>): Promise<string>;
    addMetalink(metalink: Buffer, options?: Record<string, unknown>): Promise<string>;
    remove(gid: string): Promise<string>;
    pause(gid: string): Promise<string>;
    unpause(gid: string): Promise<string>;
    forcePause(gid: string): Promise<string>;
    forceRemove(gid: string): Promise<string>;
    forceUnpause(gid: string): Promise<string>;
    tellStatus(gid: string, keys?: string[]): Promise<StatusInfo>;
    tellActive(keys?: string[]): Promise<StatusInfo[]>;
    tellWaiting(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]>;
    tellStopped(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]>;
    getGlobalStat(): Promise<GlobalStat>;
    purgeDownloadResult(): Promise<string>;
    removeDownloadResult(gid: string): Promise<string>;
    getGlobalOption(): Promise<Record<string, unknown>>;
    changeGlobalOption(options: Record<string, unknown>): Promise<string>;
    getOption(gid: string): Promise<Record<string, unknown>>;
    changeOption(gid: string, options: Record<string, unknown>): Promise<string>;
    getVersion(): Promise<VersionInfo>;
    getSessionInfo(): Promise<SessionInfo>;
    shutdown(): Promise<string>;
    forceShutdown(): Promise<string>;
    saveSession(): Promise<string>;
    on(event: WsEventName | 'reconnecting' | 'close', handler: (...args: unknown[]) => void): this;
    close(): Promise<void>;
    destroy(): void;
}

declare class Aria2Error extends Error {
    readonly code: number;
    constructor(message: string, code?: number);
}
declare class ConnectionError extends Aria2Error {
    constructor(message: string);
}
declare class AuthError extends Aria2Error {
    constructor(message: string);
}
declare class RpcError extends Aria2Error {
    constructor(message: string, code: number);
}
declare class TimeoutError extends Aria2Error {
    constructor(message: string);
}

declare class Aria2EventEmitter extends EventEmitter {
    private wsUrl;
    private ws;
    private reconnectAttempts;
    private reconnectTimer;
    private closed;
    private connectPromise;
    constructor(wsUrl: string, _options?: ClientOptions);
    connect(): Promise<void>;
    private doConnect;
    private setupMessageHandler;
    private setupCloseHandler;
    private attemptReconnect;
    close(): Promise<void>;
}

export { Aria2Client, Aria2Error, Aria2EventEmitter, AuthError, type ClientOptions, ConnectionError, type DownloadEvent, DownloadStatus, EventType, type FileInfo, type GlobalStat, RpcError, type SessionInfo, type StatusInfo, TimeoutError, type UriEntry, type VersionInfo };
