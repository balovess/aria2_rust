export { Aria2Client } from './client.js';
export type {
  StatusInfo,
  GlobalStat,
  VersionInfo,
  SessionInfo,
  FileInfo,
  UriEntry,
  ServerInfo,
  ServerInfoIndex,
  PeerInfo,
  DownloadEvent,
  ClientOptions,
} from './types.js';
export { EventType, DownloadStatus } from './types.js';
export { Aria2Error, ConnectionError, AuthError, RpcError, TimeoutError } from './errors.js';
export { Aria2EventEmitter } from './events.js';
