# @aria2-rust/client

Production-grade Node.js/TypeScript SDK for aria2-rust JSON-RPC & WebSocket interface. Complete RPC coverage with full type safety, automatic reconnection, and comprehensive test suite.

## Features

- ✅ **Complete RPC Coverage** - All 25 aria2 RPC methods
- ✅ **Dual Transport** - HTTP JSON-RPC and WebSocket support
- ✅ **Type Safe** - Full TypeScript types with strict mode
- ✅ **Error Handling** - Structured error hierarchy
- ✅ **Auto Reconnection** - Exponential backoff strategy
- ✅ **Event System** - EventEmitter-based event subscription
- ✅ **Modern API** - Promise-based async/await
- ✅ **Authentication** - Token and Basic auth support
- ✅ **Production Ready** - Unit + Integration + E2E tests
- ✅ **Dual Format** - ESM and CommonJS support

## Installation

```bash
npm install @aria2-rust/client
```

Or from source:

```bash
cd bindings/nodejs
npm install
npm run build
```

## Quick Start

### Basic Usage

```typescript
import { Aria2Client } from '@aria2-rust/client';

async function main() {
  const client = new Aria2Client('http://localhost:6800/jsonrpc');
  
  try {
    // Add a download task
    const gid = await client.addUri(['http://example.com/file.zip']);
    console.log(`Download started: ${gid}`);
    
    // Check status
    const status = await client.tellStatus(gid);
    console.log(`Status: ${status.status}, Progress: ${status.completedLength}/${status.totalLength}`);
  } finally {
    await client.close();
  }
}

main();
```

### With Authentication

```typescript
import { Aria2Client } from '@aria2-rust/client';

// Token authentication
const client = new Aria2Client('http://localhost:6800/jsonrpc', {
  token: 'my-secret-token'
});

// The token is automatically prepended to all RPC calls
const gid = await client.addUri(['http://example.com/file.zip']);
```

### Event Subscription

```typescript
import { Aria2Client } from '@aria2-rust/client';

async function main() {
  const client = new Aria2Client('ws://localhost:6800/jsonrpc');
  
  // Subscribe to download events
  client.on('downloadStart', (event) => {
    console.log(`Download started: ${event.gid}`);
  });
  
  client.on('downloadComplete', (event) => {
    console.log(`Download complete: ${event.gid}`);
  });
  
  client.on('downloadError', (event) => {
    console.error(`Download error: ${event.gid}, Code: ${event.errorCode}`);
  });
  
  // Add a task to trigger events
  await client.addUri(['http://example.com/file.zip']);
  
  // Keep the process running
  await new Promise(resolve => setTimeout(resolve, 60000));
  
  await client.close();
}

main();
```

## API Reference

### Aria2Client

#### Constructor

```typescript
new Aria2Client(url?: string, options?: ClientOptions)
```

**Parameters:**
- `url` - RPC endpoint URL (default: `'http://localhost:6800/jsonrpc'`)
- `options` - Configuration options

**ClientOptions:**
```typescript
interface ClientOptions {
  token?: string;      // Authentication token
  timeout?: number;    // Request timeout in ms (default: 30000)
  secret?: string;     // Alternative token parameter
}
```

#### Methods

All RPC methods return Promises and follow aria2 specification:

**Task Management:**
- `addUri(uris: string[], options?: Record<string, unknown>): Promise<string>`
- `addTorrent(torrent: Buffer, options?: Record<string, unknown>): Promise<string>`
- `addMetalink(metalink: Buffer, options?: Record<string, unknown>): Promise<string>`
- `remove(gid: string): Promise<string>`
- `pause(gid: string): Promise<string>`
- `unpause(gid: string): Promise<string>`
- `forcePause(gid: string): Promise<string>`
- `forceRemove(gid: string): Promise<string>`
- `forceUnpause(gid: string): Promise<string>`

**Status Queries:**
- `tellStatus(gid: string, keys?: string[]): Promise<StatusInfo>`
- `tellActive(keys?: string[]): Promise<StatusInfo[]>`
- `tellWaiting(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]>`
- `tellStopped(offset: number, num: number, keys?: string[]): Promise<StatusInfo[]>`
- `getGlobalStat(): Promise<GlobalStat>`

**Options:**
- `getGlobalOption(): Promise<Record<string, unknown>>`
- `changeGlobalOption(options: Record<string, unknown>): Promise<string>`
- `getOption(gid: string): Promise<Record<string, unknown>>`
- `changeOption(gid: string, options: Record<string, unknown>): Promise<string>`

**System:**
- `getVersion(): Promise<VersionInfo>`
- `getSessionInfo(): Promise<SessionInfo>`
- `shutdown(): Promise<string>`
- `forceShutdown(): Promise<string>`
- `saveSession(): Promise<string>`
- `purgeDownloadResult(): Promise<string>`
- `removeDownloadResult(gid: string): Promise<string>`

**Event Subscription:**
- `on(event: string, handler: Function): this`
- `off(event: string, handler: Function): this`
- `once(event: string, handler: Function): this`

**Lifecycle:**
- `close(): Promise<void>`
- `destroy(): void`

### Types

#### StatusInfo

```typescript
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
```

#### GlobalStat

```typescript
interface GlobalStat {
  downloadSpeed: string;
  uploadSpeed: string;
  numActive: string;
  numWaiting: string;
  numStopped: string;
  numStoppedTotal: string;
}
```

#### VersionInfo

```typescript
interface VersionInfo {
  version: string;
  enabledFeatures: string[];
}
```

#### SessionInfo

```typescript
interface SessionInfo {
  sessionId: string;
}
```

#### FileInfo

```typescript
interface FileInfo {
  index: string;
  path: string;
  length: string;
  completedLength: string;
  selected: string;
  uris: UriEntry[];
}
```

#### UriEntry

```typescript
interface UriEntry {
  uri: string;
  status: 'used' | 'waiting';
}
```

#### DownloadEvent

```typescript
interface DownloadEvent {
  type: EventType;
  gid: string;
  errorCode?: number;
  files?: unknown[];
}
```

#### Enums

```typescript
const enum EventType {
  DownloadStart = 'aria2.onDownloadStart',
  DownloadPause = 'aria2.onDownloadPause',
  DownloadStop = 'aria2.onDownloadStop',
  DownloadComplete = 'aria2.onDownloadComplete',
  DownloadError = 'aria2.onDownloadError',
  BtDownloadComplete = 'aria2.onBtDownloadComplete',
  BtDownloadError = 'aria2.onBtDownloadError',
}

const enum DownloadStatus {
  Active = 'active',
  Waiting = 'waiting',
  Paused = 'paused',
  Error = 'error',
  Complete = 'complete',
  Removed = 'removed',
}
```

### Errors

```typescript
class Aria2Error extends Error {
  code: number;
}

class ConnectionError extends Aria2Error {  // code = -2 }
class AuthError extends Aria2Error {  // code = -3 }
class RpcError extends Aria2Error {  // code from JSON-RPC response }
class TimeoutError extends Aria2Error {  // code = -4 }
```

## Examples

### Download Progress Monitoring

```typescript
import { Aria2Client, StatusInfo } from '@aria2-rust/client';

async function downloadWithProgress(url: string): Promise<StatusInfo> {
  const client = new Aria2Client();
  
  try {
    const gid = await client.addUri([url]);
    
    while (true) {
      const status = await client.tellStatus(gid);
      const progress = Number(status.completedLength) / Number(status.totalLength) * 100;
      const speed = Number(status.downloadSpeed) / 1024; // KB/s
      
      console.log(`Progress: ${progress.toFixed(1)}%, Speed: ${speed.toFixed(1)} KB/s`);
      
      if (['complete', 'error', 'removed'].includes(status.status)) {
        return status;
      }
      
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
  } finally {
    await client.close();
  }
}

downloadWithProgress('http://example.com/largefile.zip');
```

### Batch Download

```typescript
import { Aria2Client } from '@aria2-rust/client';

async function batchDownload(urls: string[], maxConcurrent = 5) {
  const client = new Aria2Client();
  
  try {
    // Add multiple tasks
    const gidPromises = urls.map(url => client.addUri([url]));
    const gids = await Promise.all(gidPromises);
    
    console.log(`Added ${gids.length} tasks`);
    
    // Monitor all tasks
    while (true) {
      const statusPromises = gids.map(gid => client.tellStatus(gid));
      const statuses = await Promise.all(statusPromises);
      
      const active = statuses.filter(s => s.status === 'active').length;
      const complete = statuses.filter(s => s.status === 'complete').length;
      const error = statuses.filter(s => s.status === 'error').length;
      
      console.log(`Active: ${active}, Complete: ${complete}, Error: ${error}`);
      
      if (complete + error === gids.length) {
        break;
      }
      
      await new Promise(resolve => setTimeout(resolve, 2000));
    }
  } finally {
    await client.close();
  }
}

const urls = [
  'http://example.com/file1.zip',
  'http://example.com/file2.zip',
  'http://example.com/file3.zip',
];

batchDownload(urls);
```

### Torrent Download

```typescript
import { Aria2Client } from '@aria2-rust/client';
import { promises as fs } from 'fs';

async function downloadTorrent(torrentPath: string) {
  const client = new Aria2Client();
  
  try {
    const torrentData = await fs.readFile(torrentPath);
    const gid = await client.addTorrent(torrentData);
    
    console.log(`Torrent added: ${gid}`);
    
    // Monitor progress
    while (true) {
      const status = await client.tellStatus(gid);
      
      if (['complete', 'error'].includes(status.status)) {
        console.log(`Torrent finished: ${status.status}`);
        break;
      }
      
      await new Promise(resolve => setTimeout(resolve, 5000));
    }
  } finally {
    await client.close();
  }
}

downloadTorrent('example.torrent');
```

### Event-Driven Download

```typescript
import { Aria2Client } from '@aria2-rust/client';

async function eventDrivenDownload() {
  const client = new Aria2Client('ws://localhost:6800/jsonrpc');
  
  // Track downloads
  const downloads = new Map<string, { start: number, progress: number }>();
  
  client.on('downloadStart', (event) => {
    console.log(`⬇️  Started: ${event.gid}`);
    downloads.set(event.gid, { start: Date.now(), progress: 0 });
  });
  
  client.on('downloadComplete', (event) => {
    const info = downloads.get(event.gid);
    const duration = info ? Date.now() - info.start : 0;
    console.log(`✅ Complete: ${event.gid} (${duration}ms)`);
    downloads.delete(event.gid);
  });
  
  client.on('downloadError', (event) => {
    console.error(`❌ Error: ${event.gid}, Code: ${event.errorCode}`);
    downloads.delete(event.gid);
  });
  
  // Add downloads
  await client.addUri(['http://example.com/file1.zip']);
  await client.addUri(['http://example.com/file2.zip']);
  
  // Wait for completion
  await new Promise(resolve => setTimeout(resolve, 120000));
  
  await client.close();
}

eventDrivenDownload();
```

## Testing

### Run All Tests

```bash
# Unit tests
npm run test:unit

# Integration tests (requires mock server)
npm run test:integration

# E2E tests (requires aria2-rust binary)
npm run test:e2e

# All tests
npm test
```

### Run Type Checker

```bash
npm run typecheck
```

### Build

```bash
npm run build
```

## Development

### Setup Development Environment

```bash
npm install
```

### Project Structure

```
bindings/nodejs/
├── src/
│   ├── index.ts          # Public API exports
│   ├── client.ts         # Aria2Client implementation
│   ├── types.ts          # TypeScript interfaces and enums
│   ├── errors.ts         # Error hierarchy
│   ├── transport.ts      # HTTP and WebSocket transports
│   └── events.ts         # EventEmitter implementation
├── tests/
│   ├── unit/             # Unit tests
│   ├── integration/      # Integration tests
│   └── e2e/              # E2E tests
├── dist/                 # Build output
├── package.json
├── tsconfig.json
└── vitest.config.ts
```

## Requirements

- Node.js 18+
- ws >= 8.16

### Development Dependencies

- typescript >= 5.3
- tsup >= 8.0
- vitest >= 1.2
- @types/ws >= 8.5
- @types/node >= 20.11

## License

GPL-2.0-or-later

## Contributing

1. Fork the repository
2. Create a feature branch
3. Run tests: `npm test`
4. Submit a pull request

## Support

For issues and feature requests, please open an issue on the GitHub repository.
