from __future__ import annotations

import re
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Dict, List, Optional


def _camel_to_snake(name: str) -> str:
    s1 = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s1).lower()


def _convert_keys(data: Dict[str, Any]) -> Dict[str, Any]:
    return {_camel_to_snake(k): v for k, v in data.items()}


class EventType(str, Enum):
    DOWNLOAD_START = "DOWNLOAD_START"
    DOWNLOAD_PAUSE = "DOWNLOAD_PAUSE"
    DOWNLOAD_STOP = "DOWNLOAD_STOP"
    DOWNLOAD_COMPLETE = "DOWNLOAD_COMPLETE"
    DOWNLOAD_ERROR = "DOWNLOAD_ERROR"
    BT_DOWNLOAD_COMPLETE = "BT_DOWNLOAD_COMPLETE"
    BT_DOWNLOAD_ERROR = "BT_DOWNLOAD_ERROR"


class DownloadStatus(str, Enum):
    ACTIVE = "active"
    WAITING = "waiting"
    PAUSED = "paused"
    ERROR = "error"
    COMPLETE = "complete"
    REMOVED = "removed"


_EVENT_METHOD_MAP: Dict[str, EventType] = {
    "aria2.onDownloadStart": EventType.DOWNLOAD_START,
    "aria2.onDownloadPause": EventType.DOWNLOAD_PAUSE,
    "aria2.onDownloadStop": EventType.DOWNLOAD_STOP,
    "aria2.onDownloadComplete": EventType.DOWNLOAD_COMPLETE,
    "aria2.onDownloadError": EventType.DOWNLOAD_ERROR,
    "aria2.onBtDownloadComplete": EventType.BT_DOWNLOAD_COMPLETE,
    "aria2.onBtDownloadError": EventType.BT_DOWNLOAD_ERROR,
}


@dataclass
class UriEntry:
    uri: Optional[str] = None
    status: Optional[str] = None

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> UriEntry:
        converted = _convert_keys(data)
        return cls(
            uri=converted.get("uri"),
            status=converted.get("status"),
        )


@dataclass
class FileInfo:
    index: Optional[str] = None
    path: Optional[str] = None
    length: Optional[str] = None
    completed_length: Optional[str] = None
    selected: Optional[str] = None
    uris: List[UriEntry] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> FileInfo:
        converted = _convert_keys(data)
        uris_data = converted.get("uris") or []
        uris = [UriEntry.from_dict(u) if isinstance(u, dict) else u for u in uris_data]
        return cls(
            index=converted.get("index"),
            path=converted.get("path"),
            length=converted.get("length"),
            completed_length=converted.get("completed_length"),
            selected=converted.get("selected"),
            uris=uris,
        )


@dataclass
class StatusInfo:
    gid: Optional[str] = None
    total_length: Optional[str] = None
    completed_length: Optional[str] = None
    upload_length: Optional[str] = None
    download_speed: Optional[str] = None
    upload_speed: Optional[str] = None
    error_code: Optional[str] = None
    status: Optional[str] = None
    dir: Optional[str] = None
    files: List[FileInfo] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> StatusInfo:
        converted = _convert_keys(data)
        files_data = converted.get("files") or []
        files = [FileInfo.from_dict(f) if isinstance(f, dict) else f for f in files_data]
        return cls(
            gid=converted.get("gid"),
            total_length=converted.get("total_length"),
            completed_length=converted.get("completed_length"),
            upload_length=converted.get("upload_length"),
            download_speed=converted.get("download_speed"),
            upload_speed=converted.get("upload_speed"),
            error_code=converted.get("error_code"),
            status=converted.get("status"),
            dir=converted.get("dir"),
            files=files,
        )


@dataclass
class GlobalStat:
    download_speed: Optional[str] = None
    upload_speed: Optional[str] = None
    num_active: Optional[str] = None
    num_waiting: Optional[str] = None
    num_stopped: Optional[str] = None
    num_stopped_total: Optional[str] = None

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> GlobalStat:
        converted = _convert_keys(data)
        return cls(
            download_speed=converted.get("download_speed"),
            upload_speed=converted.get("upload_speed"),
            num_active=converted.get("num_active"),
            num_waiting=converted.get("num_waiting"),
            num_stopped=converted.get("num_stopped"),
            num_stopped_total=converted.get("num_stopped_total"),
        )


@dataclass
class VersionInfo:
    version: Optional[str] = None
    enabled_features: List[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> VersionInfo:
        converted = _convert_keys(data)
        features = converted.get("enabled_features") or []
        return cls(
            version=converted.get("version"),
            enabled_features=list(features) if isinstance(features, (list, tuple)) else [],
        )


@dataclass
class SessionInfo:
    session_id: Optional[str] = None

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> SessionInfo:
        converted = _convert_keys(data)
        return cls(session_id=converted.get("session_id"))


@dataclass
class DownloadEvent:
    event_type: EventType
    gid: Optional[str] = None
    error_code: Optional[str] = None
    files: Optional[List[FileInfo]] = None

    @classmethod
    def from_rpc_notification(cls, method: str, params: Dict[str, Any]) -> DownloadEvent:
        event_type = _EVENT_METHOD_MAP.get(method)
        if event_type is None:
            event_type = EventType.DOWNLOAD_START
        converted = _convert_keys(params)
        files_data = converted.get("files")
        files = None
        if files_data and isinstance(files_data, list):
            files = [FileInfo.from_dict(f) if isinstance(f, dict) else f for f in files_data]
        return cls(
            event_type=event_type,
            gid=converted.get("gid"),
            error_code=converted.get("error_code"),
            files=files,
        )
