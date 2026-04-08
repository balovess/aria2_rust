from .client import Aria2Client
from .errors import Aria2Error, AuthError, ConnectionError, RpcError, TimeoutError
from .events import EventSubscriber
from .types import (
    DownloadEvent,
    DownloadStatus,
    EventType,
    FileInfo,
    GlobalStat,
    SessionInfo,
    StatusInfo,
    UriEntry,
    VersionInfo,
)

__all__ = [
    "Aria2Client",
    "Aria2Error",
    "AuthError",
    "ConnectionError",
    "RpcError",
    "TimeoutError",
    "EventSubscriber",
    "DownloadEvent",
    "DownloadStatus",
    "EventType",
    "FileInfo",
    "GlobalStat",
    "SessionInfo",
    "StatusInfo",
    "UriEntry",
    "VersionInfo",
]
