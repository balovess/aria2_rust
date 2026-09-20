export interface StatusInfo {
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

export interface GlobalStat {
  downloadSpeed: string;
  uploadSpeed: string;
  numActive: string;
  numWaiting: string;
  numStopped: string;
  numStoppedTotal: string;
}

export interface VersionInfo {
  version: string;
  enabledFeatures: string[];
}

export interface SessionInfo {
  sessionId: string;
}

export interface FileInfo {
  index: string;
  path: string;
  length: string;
  completedLength: string;
  selected: string;
  uris: UriEntry[];
}

export interface UriEntry {
  uri: string;
  status: 'used' | 'waiting';
}

/** Queue-position operation accepted by aria2.changePosition. */
export const enum PositionMode {
  SetFromStart = 'POS_SET',
  MoveFromStart = 'POS_CUR',
  SetFromEnd = 'POS_END',
}

export interface ServerInfo {
  uri: string;
  currentUri: string;
  downloadSpeed: string;
}

export interface ServerInfoIndex {
  index: string;
  servers: ServerInfo[];
}

export interface PeerInfo {
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

export interface TrackerInfo {
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

export interface DhtStatus {
  state: string;
  totalNodes: string;
  goodNodes: string;
  pendingTransactions: string;
}

export interface DownloadEvent {
  type: EventType;
  gid: string;
  errorCode?: number;
  files?: FileInfo[];
}

export const enum EventType {
  DownloadStart = 'aria2.onDownloadStart',
  DownloadPause = 'aria2.onDownloadPause',
  DownloadStop = 'aria2.onDownloadStop',
  DownloadComplete = 'aria2.onDownloadComplete',
  DownloadError = 'aria2.onDownloadError',
  BtDownloadComplete = 'aria2.onBtDownloadComplete',
  BtDownloadError = 'aria2.onBtDownloadError',
}

export const enum DownloadStatus {
  Active = 'active',
  Waiting = 'waiting',
  Paused = 'paused',
  Error = 'error',
  Complete = 'complete',
  Removed = 'removed',
}

export interface ClientOptions {
  token?: string;
  timeout?: number;
  secret?: string;
}
