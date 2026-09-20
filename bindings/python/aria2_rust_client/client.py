from __future__ import annotations

import base64
from typing import Any, Callable, Dict, List, Optional, TypeVar, Union

from typing_extensions import Self

from .errors import Aria2Error
from .events import EventSubscriber
from .transport import HttpTransport, Transport, WebSocketTransport
from .types import (
    DhtStatus,
    EventType,
    FileInfo,
    GlobalStat,
    PeerInfo,
    PositionMode,
    ServerInfoIndex,
    SessionInfo,
    StatusInfo,
    TrackerInfo,
    UriEntry,
    VersionInfo,
)


def _http_to_ws(url: str) -> str:
    if url.startswith("https://"):
        return "wss://" + url[8:]
    if url.startswith("http://"):
        return "ws://" + url[7:]
    return url


T = TypeVar("T")


def _parse_dict_list(
    result: Any, method: str, parser: Callable[[Dict[str, Any]], T]
) -> List[T]:
    """Decode an RPC object list without silently dropping malformed entries."""
    if not isinstance(result, list):
        raise Aria2Error(f"Unexpected result type for {method}: {type(result)}")
    for index, item in enumerate(result):
        if not isinstance(item, dict):
            raise Aria2Error(
                f"Unexpected item type for {method} at index {index}: {type(item)}"
            )
    return [parser(item) for item in result]


def _parse_dict_result(result: Any, method: str) -> Dict[str, Any]:
    """Decode an RPC object result without treating malformed data as empty."""
    if not isinstance(result, dict):
        raise Aria2Error(f"Unexpected result type for {method}: {type(result)}")
    return result


def _parse_string_list(
    result: Any, method: str, expected_length: Optional[int] = None
) -> List[str]:
    """Decode a wire string list without coercing malformed values."""
    if not isinstance(result, list):
        raise Aria2Error(f"Unexpected result type for {method}: {type(result)}")
    if expected_length is not None and len(result) != expected_length:
        raise Aria2Error(
            f"Unexpected result length for {method}: expected {expected_length}, "
            f"got {len(result)}"
        )
    for index, item in enumerate(result):
        if not isinstance(item, str):
            raise Aria2Error(
                f"Unexpected item type for {method} at index {index}: {type(item)}"
            )
    return result


def _parse_change_uri_counts(result: Any) -> List[str]:
    """Decode aria2's two changeUri counts from numeric or string JSON values."""
    if not isinstance(result, list):
        raise Aria2Error(f"Unexpected result type for changeUri: {type(result)}")
    if len(result) != 2:
        raise Aria2Error(
            f"Unexpected result length for changeUri: expected 2, got {len(result)}"
        )
    counts: List[str] = []
    for index, item in enumerate(result):
        if isinstance(item, bool):
            raise Aria2Error(
                f"Unexpected item type for changeUri at index {index}: {type(item)}"
            )
        if isinstance(item, int) and item >= 0:
            counts.append(str(item))
        elif isinstance(item, str) and item.isascii() and item.isdecimal():
            counts.append(item)
        else:
            raise Aria2Error(
                f"Unexpected item type for changeUri at index {index}: {type(item)}"
            )
    return counts


def _parse_string_result(result: Any, method: str) -> str:
    """Decode a standard aria2 string result without coercion."""
    if not isinstance(result, str):
        raise Aria2Error(f"Unexpected result type for {method}: {type(result)}")
    return result


class Aria2Client:
    def __init__(
        self,
        url: str = "http://localhost:6800/jsonrpc",
        token: Optional[str] = None,
        timeout: float = 30.0,
    ) -> None:
        self._url = url
        self._token = token
        self._timeout = timeout
        self._transport: Transport

        if url.startswith("ws://") or url.startswith("wss://"):
            self._transport = WebSocketTransport(url, token, timeout)
        else:
            self._transport = HttpTransport(url, token, timeout)

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *args: Any) -> None:
        await self.close()

    async def _call(self, method: str, params: Optional[list] = None) -> Any:
        return await self._transport.send_request(method, params or [])

    async def call(
        self, method: str, params: Optional[List[Any]] = None
    ) -> Any:
        """Call an arbitrary JSON-RPC method, including fork-specific extensions."""
        return await self._call(method, params)

    async def add_uri(
        self,
        uris: List[str],
        options: Optional[Dict] = None,
        position: Optional[int] = None,
    ) -> str:
        params: list = [uris]
        if options is not None or position is not None:
            params.append(options or {})
        if position is not None:
            params.append(position)
        result = await self._call("aria2.addUri", params)
        return _parse_string_result(result, "addUri")

    async def add_torrent(
        self,
        torrent: bytes,
        options: Optional[Dict] = None,
        web_seed_uris: Optional[List[str]] = None,
        position: Optional[int] = None,
    ) -> str:
        encoded = base64.b64encode(torrent).decode("ascii")
        params: list = [encoded]
        if web_seed_uris is not None or options is not None or position is not None:
            params.append(web_seed_uris or [])
        if options is not None or position is not None:
            params.append(options or {})
        if position is not None:
            params.append(position)
        result = await self._call("aria2.addTorrent", params)
        return _parse_string_result(result, "addTorrent")

    async def add_metalink(
        self,
        metalink: bytes,
        options: Optional[Dict] = None,
        position: Optional[int] = None,
    ) -> List[str]:
        encoded = base64.b64encode(metalink).decode("ascii")
        params: list = [encoded]
        if options is not None:
            params.append(options)
        elif position is not None:
            params.append({})
        if position is not None:
            params.append(position)
        result = await self._call("aria2.addMetalink", params)
        return _parse_string_list(result, "addMetalink")

    async def remove(self, gid: str) -> str:
        result = await self._call("aria2.remove", [gid])
        return _parse_string_result(result, "remove")

    async def pause(self, gid: str) -> str:
        result = await self._call("aria2.pause", [gid])
        return _parse_string_result(result, "pause")

    async def unpause(self, gid: str) -> str:
        result = await self._call("aria2.unpause", [gid])
        return _parse_string_result(result, "unpause")

    async def force_pause(self, gid: str) -> str:
        result = await self._call("aria2.forcePause", [gid])
        return _parse_string_result(result, "forcePause")

    async def force_remove(self, gid: str) -> str:
        result = await self._call("aria2.forceRemove", [gid])
        return _parse_string_result(result, "forceRemove")

    async def pause_all(self) -> str:
        result = await self._call("aria2.pauseAll")
        return _parse_string_result(result, "pauseAll")

    async def force_pause_all(self) -> str:
        result = await self._call("aria2.forcePauseAll")
        return _parse_string_result(result, "forcePauseAll")

    async def unpause_all(self) -> str:
        result = await self._call("aria2.unpauseAll")
        return _parse_string_result(result, "unpauseAll")

    async def change_position(
        self, gid: str, position: int, mode: Union[PositionMode, str]
    ) -> int:
        result = await self._call("aria2.changePosition", [gid, position, mode])
        if isinstance(result, int) and not isinstance(result, bool):
            return result
        if isinstance(result, str) and result.isascii() and result.isdecimal():
            return int(result)
        raise Aria2Error(f"Unexpected result type for changePosition: {type(result)}")

    async def change_uri(
        self,
        gid: str,
        file_index: int,
        delete_uris: List[str],
        add_uris: List[str],
        position: Optional[int] = None,
    ) -> List[str]:
        params: list = [gid, file_index, delete_uris, add_uris]
        if position is not None:
            params.append(position)
        result = await self._call("aria2.changeUri", params)
        return _parse_change_uri_counts(result)

    async def tell_status(
        self, gid: str, keys: Optional[List[str]] = None
    ) -> StatusInfo:
        params: list = [gid]
        if keys is not None:
            params.append(keys)
        result = await self._call("aria2.tellStatus", params)
        if isinstance(result, dict):
            return StatusInfo.from_dict(result)
        raise Aria2Error(f"Unexpected result type for tellStatus: {type(result)}")

    async def get_files(self, gid: str) -> List[FileInfo]:
        """Return the file metadata associated with a download GID.

        This is the Python binding for aria2's ``aria2.getFiles`` method.
        For HTTP/FTP downloads the length may remain unknown until the
        metadata probe has completed.  Magnet downloads likewise require
        metadata exchange before their file list is complete.
        """
        result = await self._call("aria2.getFiles", [gid])
        return _parse_dict_list(result, "getFiles", FileInfo.from_dict)

    async def get_uris(self, gid: str) -> List[UriEntry]:
        result = await self._call("aria2.getUris", [gid])
        return _parse_dict_list(result, "getUris", UriEntry.from_dict)

    async def get_servers(self, gid: str) -> List[ServerInfoIndex]:
        result = await self._call("aria2.getServers", [gid])
        return _parse_dict_list(result, "getServers", ServerInfoIndex.from_dict)

    async def get_peers(self, gid: str) -> List[PeerInfo]:
        result = await self._call("aria2.getPeers", [gid])
        return _parse_dict_list(result, "getPeers", PeerInfo.from_dict)

    async def get_trackers(self, gid: str) -> List[TrackerInfo]:
        result = await self._call("aria2.getTrackers", [gid])
        return _parse_dict_list(result, "getTrackers", TrackerInfo.from_dict)

    async def get_dht_status(self) -> DhtStatus:
        result = await self._call("aria2.getDhtStatus")
        if isinstance(result, dict):
            return DhtStatus.from_dict(result)
        raise Aria2Error(f"Unexpected result type for getDhtStatus: {type(result)}")

    async def tell_active(
        self, keys: Optional[List[str]] = None
    ) -> List[StatusInfo]:
        params: list = []
        if keys is not None:
            params.append(keys)
        result = await self._call("aria2.tellActive", params)
        return _parse_dict_list(result, "tellActive", StatusInfo.from_dict)

    async def tell_waiting(
        self, offset: int, num: int, keys: Optional[List[str]] = None
    ) -> List[StatusInfo]:
        params: list = [offset, num]
        if keys is not None:
            params.append(keys)
        result = await self._call("aria2.tellWaiting", params)
        return _parse_dict_list(result, "tellWaiting", StatusInfo.from_dict)

    async def tell_stopped(
        self, offset: int, num: int, keys: Optional[List[str]] = None
    ) -> List[StatusInfo]:
        params: list = [offset, num]
        if keys is not None:
            params.append(keys)
        result = await self._call("aria2.tellStopped", params)
        return _parse_dict_list(result, "tellStopped", StatusInfo.from_dict)

    async def get_global_stat(self) -> GlobalStat:
        result = await self._call("aria2.getGlobalStat")
        if isinstance(result, dict):
            return GlobalStat.from_dict(result)
        raise Aria2Error(f"Unexpected result type for getGlobalStat: {type(result)}")

    async def purge_download_result(self) -> str:
        result = await self._call("aria2.purgeDownloadResult")
        return _parse_string_result(result, "purgeDownloadResult")

    async def remove_download_result(self, gid: str) -> str:
        result = await self._call("aria2.removeDownloadResult", [gid])
        return _parse_string_result(result, "removeDownloadResult")

    async def get_global_option(self) -> Dict:
        result = await self._call("aria2.getGlobalOption")
        return _parse_dict_result(result, "getGlobalOption")

    async def change_global_option(self, options: Dict) -> str:
        result = await self._call("aria2.changeGlobalOption", [options])
        return _parse_string_result(result, "changeGlobalOption")

    async def get_option(self, gid: str) -> Dict:
        result = await self._call("aria2.getOption", [gid])
        return _parse_dict_result(result, "getOption")

    async def change_option(self, gid: str, options: Dict) -> str:
        result = await self._call("aria2.changeOption", [gid, options])
        return _parse_string_result(result, "changeOption")

    async def get_version(self) -> VersionInfo:
        result = await self._call("aria2.getVersion")
        if isinstance(result, dict):
            return VersionInfo.from_dict(result)
        raise Aria2Error(f"Unexpected result type for getVersion: {type(result)}")

    async def get_session_info(self) -> SessionInfo:
        result = await self._call("aria2.getSessionInfo")
        if isinstance(result, dict):
            return SessionInfo.from_dict(result)
        raise Aria2Error(f"Unexpected result type for getSessionInfo: {type(result)}")

    async def shutdown(self) -> str:
        result = await self._call("aria2.shutdown")
        return _parse_string_result(result, "shutdown")

    async def force_shutdown(self) -> str:
        result = await self._call("aria2.forceShutdown")
        return _parse_string_result(result, "forceShutdown")

    async def save_session(self) -> str:
        result = await self._call("aria2.saveSession")
        return _parse_string_result(result, "saveSession")

    async def update_browser_context(self, context: Any) -> str:
        result = await self._call("aria2.updateBrowserContext", [context])
        return _parse_string_result(result, "updateBrowserContext")

    async def clear_browser_context(self) -> str:
        result = await self._call("aria2.clearBrowserContext")
        return _parse_string_result(result, "clearBrowserContext")

    async def system_multicall(self, calls: List[Dict[str, Any]]) -> List[Any]:
        result = await self._call("system.multicall", [calls])
        if isinstance(result, list):
            return result
        raise Aria2Error(f"Unexpected result type for system.multicall: {type(result)}")

    async def system_list_methods(self) -> List[str]:
        result = await self._call("system.listMethods")
        return _parse_string_list(result, "system.listMethods")

    async def system_list_notifications(self) -> List[str]:
        result = await self._call("system.listNotifications")
        return _parse_string_list(result, "system.listNotifications")

    async def subscribe_events(
        self, filter: Optional[List[EventType]] = None
    ) -> EventSubscriber:
        ws_url = _http_to_ws(self._url)
        subscriber = EventSubscriber(ws_url, self._token, filter)
        await subscriber.start()
        return subscriber

    async def close(self) -> None:
        await self._transport.close()
