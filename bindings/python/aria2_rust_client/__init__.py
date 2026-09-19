from .client import Aria2Client
from .errors import Aria2Error, AuthError, ConnectionError, RpcError, TimeoutError
from .events import EventSubscriber
from .types import (
    DhtStatus,
    DownloadEvent,
    DownloadStatus,
    EventType,
    FileInfo,
    GlobalStat,
    PeerInfo,
    ServerInfo,
    ServerInfoIndex,
    SessionInfo,
    StatusInfo,
    TrackerInfo,
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
    "DhtStatus",
    "EventType",
    "FileInfo",
    "GlobalStat",
    "PeerInfo",
    "ServerInfo",
    "ServerInfoIndex",
    "SessionInfo",
    "StatusInfo",
    "TrackerInfo",
    "UriEntry",
    "VersionInfo",
]
