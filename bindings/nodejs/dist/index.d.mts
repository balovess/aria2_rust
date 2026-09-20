import { EventEmitter } from 'events';

interface StatusInfo {
    gid: string;
    totalLength?: string;
    completedLength?: string;
    uploadLength?: string;
    downloadSpeed?: string;
    uploadSpeed?: string;
    connections?: string;
    errorCode?: string;
    errorMessage?: string;
    status: DownloadStatus;
    dir?: string;
    files?: FileInfo[];
    bittorrent?: Record<string, unknown>;
    following?: string;
    seeder?: string;
    bitfield?: string;
    pieceLength?: string;
    numPieces?: string;
    completedPieces?: string;
    missingPieces?: string;
    followedBy?: string[];
    belongsTo?: string;
    infoHash?: string;
    numSeeders?: string;
    verifiedLength?: string;
    verifyIntegrityPending?: string;
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
/** Queue-position operation accepted by aria2.changePosition. */
declare const enum PositionMode {
    SetFromStart = "POS_SET",
    MoveFromStart = "POS_CUR",
    SetFromEnd = "POS_END"
}
interface ServerInfo {
    uri: string;
    currentUri: string;
    downloadSpeed: string;
}
interface ServerInfoIndex {
    index: string;
    servers: ServerInfo[];
}
interface PeerInfo {
    peerId: string;
    ip: string;
    port: string;
    bitfield?: string;
    amChoking: string;
    peerChoking: string;
    downloadSpeed: string;
    uploadSpeed: string;
    seeder?: string;
}
interface TrackerInfo {
    uri: string;
    tier: number;
    current: boolean;
    lastAttempt: boolean;
    announceReady: boolean;
    allFailed: boolean;
    inFlight: number;
    interval: string;
    minInterval: number;
    seeders: number;
    leechers: number;
    trackerId: string;
    secondsSinceLastSuccess?: number;
}
interface DhtStatus {
    state: string;
    totalNodes: string;
    goodNodes: string;
    pendingTransactions: string;
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

declare class Aria2EventEmitter extends EventEmitter {
    private wsUrl;
    private ws;
    private pendingWs;
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

declare const WS_EVENT_NAMES: readonly ["downloadStart", "downloadPause", "downloadStop", "downloadComplete", "downloadError", "btDownloadComplete", "btDownloadError"];
type WsEventName = (typeof WS_EVENT_NAMES)[number];
declare class Aria2Client {
    private transport;
    private eventEmitter;
    private url;
    private options;
    constructor(url?: string, options?: ClientOptions);
    private ensureEventEmitter;
    private getOrCreateEventEmitter;
    /** Connect the notification WebSocket before starting a download. */
    connectEvents(): Promise<Aria2EventEmitter>;
    call<T = unknown>(method: string, params?: unknown[]): Promise<T>;
    addUri(uris: string[], options?: Record<string, unknown>, position?: number): Promise<string>;
    addTorrent(torrent: Buffer, options?: Record<string, unknown>, webSeedUris?: string[], position?: number): Promise<string>;
    addMetalink(metalink: Buffer, options?: Record<string, unknown>, position?: number): Promise<string[]>;
    remove(gid: string): Promise<string>;
    pause(gid: string): Promise<string>;
    unpause(gid: string): Promise<string>;
    forcePause(gid: string): Promise<string>;
    forceRemove(gid: string): Promise<string>;
    pauseAll(): Promise<string>;
    forcePauseAll(): Promise<string>;
    unpauseAll(): Promise<string>;
    changePosition(gid: string, position: number, mode: PositionMode): Promise<number>;
    changeUri(gid: string, fileIndex: number, deleteUris: string[], addUris: string[], position?: number): Promise<string[]>;
    tellStatus(gid: string, keys?: string[]): Promise<StatusInfo>;
    getFiles(gid: string): Promise<FileInfo[]>;
    getUris(gid: string): Promise<UriEntry[]>;
    getServers(gid: string): Promise<ServerInfoIndex[]>;
    getPeers(gid: string): Promise<PeerInfo[]>;
    getTrackers(gid: string): Promise<TrackerInfo[]>;
    getDhtStatus(): Promise<DhtStatus>;
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
    updateBrowserContext(context: unknown): Promise<string>;
    clearBrowserContext(): Promise<string>;
    systemMulticall(calls: Array<{
        methodName: string;
        params?: unknown[];
    }>): Promise<unknown[]>;
    systemListMethods(): Promise<string[]>;
    systemListNotifications(): Promise<string[]>;
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

export { Aria2Client, Aria2Error, Aria2EventEmitter, AuthError, type ClientOptions, ConnectionError, type DhtStatus, type DownloadEvent, DownloadStatus, EventType, type FileInfo, type GlobalStat, type PeerInfo, PositionMode, RpcError, type ServerInfo, type ServerInfoIndex, type SessionInfo, type StatusInfo, TimeoutError, type TrackerInfo, type UriEntry, type VersionInfo };
