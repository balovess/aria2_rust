# aria2-rust-client

Python SDK for aria2-rust JSON-RPC & WebSocket interface. Production-grade client with complete type annotations, automatic reconnection, and comprehensive test coverage.

## Features

- ✅ **Complete RPC Coverage** - All 25 aria2 RPC methods
- ✅ **Dual Transport** - HTTP JSON-RPC and WebSocket support
- ✅ **Type Safe** - Full type annotations with mypy/pyright support
- ✅ **Error Handling** - Structured exception hierarchy
- ✅ **Auto Reconnection** - Exponential backoff (1s → 2s → 4s → 8s → 16s)
- ✅ **Event Subscription** - Real-time download event notifications
- ✅ **Async/Await** - Native asyncio support
- ✅ **Context Manager** - `async with` support for automatic cleanup
- ✅ **Authentication** - Token and Basic auth support
- ✅ **Production Ready** - Unit + Integration + E2E tests

## Installation

```bash
pip install aria2-rust-client
```

Or from source:

```bash
cd bindings/python
pip install -e ".[dev]"
```

## Quick Start

### Basic Usage

```python
import asyncio
from aria2_rust_client import Aria2Client

async def main():
    async with Aria2Client("http://localhost:6800/jsonrpc") as client:
        # Add a download task
        gid = await client.add_uri(["http://example.com/file.zip"])
        print(f"Download started: {gid}")
        
        # Check status
        status = await client.tell_status(gid)
        print(f"Status: {status.status}, Progress: {status.completed_length}/{status.total_length}")

asyncio.run(main())
```

### With Authentication

```python
# Token authentication
client = Aria2Client("http://localhost:6800/jsonrpc", token="my-secret-token")

# The token is automatically prepended to all RPC calls
gid = await client.add_uri(["http://example.com/file.zip"])
```

### WebSocket Event Subscription

```python
import asyncio
from aria2_rust_client import Aria2Client, EventType

async def main():
    async with Aria2Client("ws://localhost:6800/jsonrpc") as client:
        # Subscribe to all events
        async for event in client.subscribe_events():
            print(f"Event: {event.event_type}, GID: {event.gid}")
            
        # Or filter specific event types
        async for event in client.subscribe_events(filter=[EventType.DownloadStart, EventType.DownloadComplete]):
            print(f"Download event: {event.event_type}")

asyncio.run(main())
```

## API Reference

### Aria2Client

#### Constructor

```python
Aria2Client(
    url: str = "http://localhost:6800/jsonrpc",
    token: Optional[str] = None,
    timeout: float = 30.0
)
```

**Parameters:**
- `url` - RPC endpoint URL (http:// or ws://)
- `token` - Authentication token (optional)
- `timeout` - Request timeout in seconds

#### Methods

All RPC methods are async and follow aria2 specification:

**Task Management:**
- `add_uri(uris, options=None)` - Add HTTP/FTP download
- `add_torrent(torrent, options=None)` - Add BitTorrent download
- `add_metalink(metalink, options=None)` - Add Metalink download
- `remove(gid)` - Remove download
- `pause(gid)` - Pause download
- `unpause(gid)` - Resume download
- `force_pause(gid)` - Force pause
- `force_remove(gid)` - Force remove
- `force_unpause(gid)` - Force unpause

**Status Queries:**
- `tell_status(gid, keys=None)` - Get task status
- `tell_active(keys=None)` - Get active tasks
- `tell_waiting(offset, num, keys=None)` - Get waiting tasks
- `tell_stopped(offset, num, keys=None)` - Get stopped tasks
- `get_global_stat()` - Get global statistics

**Options:**
- `get_global_option()` - Get global options
- `change_global_option(options)` - Change global options
- `get_option(gid)` - Get task options
- `change_option(gid, options)` - Change task options

**System:**
- `get_version()` - Get version info
- `get_session_info()` - Get session info
- `shutdown()` - Graceful shutdown
- `force_shutdown()` - Force shutdown
- `save_session()` - Save session
- `purge_download_result()` - Purge completed results
- `remove_download_result(gid)` - Remove specific result

**Event Subscription:**
- `subscribe_events(filter=None)` - Subscribe to download events

**Lifecycle:**
- `close()` - Close connection
- `async with` - Context manager support

### Types

#### StatusInfo

```python
@dataclass
class StatusInfo:
    gid: str
    total_length: Optional[int]
    completed_length: Optional[int]
    download_speed: Optional[int]
    upload_speed: Optional[int]
    error_code: Optional[str]
    status: DownloadStatus
    dir: Optional[str]
    files: List[FileInfo]
```

#### GlobalStat

```python
@dataclass
class GlobalStat:
    download_speed: int
    upload_speed: int
    num_active: int
    num_waiting: int
    num_stopped: int
```

#### DownloadEvent

```python
@dataclass
class DownloadEvent:
    event_type: EventType
    gid: str
    error_code: Optional[int]
    files: List
```

#### Enums

```python
class EventType:
    DOWNLOAD_START = "aria2.onDownloadStart"
    DOWNLOAD_PAUSE = "aria2.onDownloadPause"
    DOWNLOAD_STOP = "aria2.onDownloadStop"
    DOWNLOAD_COMPLETE = "aria2.onDownloadComplete"
    DOWNLOAD_ERROR = "aria2.onDownloadError"
    BT_DOWNLOAD_COMPLETE = "aria2.onBtDownloadComplete"
    BT_DOWNLOAD_ERROR = "aria2.onBtDownloadError"

class DownloadStatus:
    ACTIVE = "active"
    WAITING = "waiting"
    PAUSED = "paused"
    ERROR = "error"
    COMPLETE = "complete"
    REMOVED = "removed"
```

### Errors

```python
class Aria2Error(Exception):
    code: int
    message: str

class ConnectionError(Aria2Error):  # code = -2
class AuthError(Aria2Error):  # code = -3
class RpcError(Aria2Error):  # code from JSON-RPC response
class TimeoutError(Aria2Error):  # code = -4
```

## Examples

### Download Progress Monitoring

```python
import asyncio
from aria2_rust_client import Aria2Client

async def download_with_progress(url, output_path=None):
    async with Aria2Client() as client:
        gid = await client.add_uri([url])
        
        while True:
            status = await client.tell_status(gid)
            progress = int(status.completed_length) / int(status.total_length) * 100
            speed = int(status.download_speed) / 1024  # KB/s
            
            print(f"Progress: {progress:.1f}%, Speed: {speed:.1f} KB/s")
            
            if status.status in ['complete', 'error', 'removed']:
                break
                
            await asyncio.sleep(1)
        
        return status

asyncio.run(download_with_progress("http://example.com/largefile.zip"))
```

### Batch Download

```python
import asyncio
from aria2_rust_client import Aria2Client

async def batch_download(urls, max_concurrent=5):
    async with Aria2Client() as client:
        # Add multiple tasks
        tasks = [client.add_uri([url]) for url in urls]
        gids = await asyncio.gather(*tasks)
        
        print(f"Added {len(gids)} tasks")
        
        # Monitor all tasks
        while True:
            statuses = await asyncio.gather(*[client.tell_status(gid) for gid in gids])
            
            active = sum(1 for s in statuses if s.status == 'active')
            complete = sum(1 for s in statuses if s.status == 'complete')
            error = sum(1 for s in statuses if s.status == 'error')
            
            print(f"Active: {active}, Complete: {complete}, Error: {error}")
            
            if complete + error == len(gids):
                break
                
            await asyncio.sleep(2)

urls = [
    "http://example.com/file1.zip",
    "http://example.com/file2.zip",
    "http://example.com/file3.zip",
]

asyncio.run(batch_download(urls))
```

### Torrent Download

```python
import asyncio
from aria2_rust_client import Aria2Client

async def download_torrent(torrent_path):
    async with Aria2Client() as client:
        with open(torrent_path, 'rb') as f:
            torrent_data = f.read()
        
        gid = await client.add_torrent(torrent_data)
        print(f"Torrent added: {gid}")
        
        # Monitor progress
        while True:
            status = await client.tell_status(gid)
            if status.status in ['complete', 'error']:
                break
            await asyncio.sleep(5)

asyncio.run(download_torrent("example.torrent"))
```

## Testing

### Run All Tests

```bash
# Unit tests
pytest tests/unit/ -v

# Integration tests (requires mock server)
pytest tests/integration/ -v

# E2E tests (requires aria2-rust binary)
pytest tests/e2e/ -v

# All tests
pytest tests/ -v
```

### Test Coverage

```bash
pytest tests/ --cov=aria2_rust_client --cov-report=html
```

## Development

### Setup Development Environment

```bash
pip install -e ".[dev]"
```

### Run Type Checker

```bash
mypy aria2_rust_client
```

### Run Linter

```bash
flake8 aria2_rust_client
```

## Requirements

- Python 3.9+
- httpx >= 0.25
- websockets >= 12.0

### Development Dependencies

- pytest >= 7.0
- pytest-asyncio >= 0.21
- respx >= 0.20
- mypy (optional)
- flake8 (optional)

## License

GPL-2.0-or-later

## Contributing

1. Fork the repository
2. Create a feature branch
3. Run tests: `pytest tests/ -v`
4. Submit a pull request

## Support

For issues and feature requests, please open an issue on the GitHub repository.
