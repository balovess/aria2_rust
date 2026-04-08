from __future__ import annotations

import base64
from typing import Any, Dict, List, Optional, Self

from .errors import Aria2Error
from .events import EventSubscriber
from .transport import HttpTransport, Transport, WebSocketTransport
from .types import (
    EventType,
    GlobalStat,
    SessionInfo,
    StatusInfo,
    VersionInfo,
)


def _http_to_ws(url: str) -> str:
    if url.startswith("https://"):
        return "wss://" + url[8:]
    if url.startswith("http://"):
        return "ws://" + url[7:]
    return url


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

    async def add_uri(
        self, uris: List[str], options: Optional[Dict] = None
    ) -> str:
        params: list = [uris]
        if options is not None:
            params.append(options)
        result = await self._call("aria2.addUri", params)
        return str(result)

    async def add_torrent(
        self, torrent: bytes, options: Optional[Dict] = None
    ) -> str:
        encoded = base64.b64encode(torrent).decode("ascii")
        params: list = [encoded]
        if options is not None:
            params.append(options)
        result = await self._call("aria2.addTorrent", params)
        return str(result)

    async def add_metalink(
        self, metalink: bytes, options: Optional[Dict] = None
    ) -> str:
        encoded = base64.b64encode(metalink).decode("ascii")
        params: list = [encoded]
        if options is not None:
            params.append(options)
        result = await self._call("aria2.addMetalink", params)
        return str(result)

    async def remove(self, gid: str) -> str:
        result = await self._call("aria2.remove", [gid])
        return str(result)

    async def pause(self, gid: str) -> str:
        result = await self._call("aria2.pause", [gid])
        return str(result)

    async def unpause(self, gid: str) -> str:
        result = await self._call("aria2.unpause", [gid])
        return str(result)

    async def force_pause(self, gid: str) -> str:
        result = await self._call("aria2.forcePause", [gid])
        return str(result)

    async def force_remove(self, gid: str) -> str:
        result = await self._call("aria2.forceRemove", [gid])
        return str(result)

    async def force_unpause(self, gid: str) -> str:
        result = await self._call("aria2.forceUnpause", [gid])
        return str(result)

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

    async def tell_active(
        self, keys: Optional[List[str]] = None
    ) -> List[StatusInfo]:
        params: list = []
        if keys is not None:
            params.append(keys)
        result = await self._call("aria2.tellActive", params)
        if isinstance(result, list):
            return [StatusInfo.from_dict(item) for item in result if isinstance(item, dict)]
        raise Aria2Error(f"Unexpected result type for tellActive: {type(result)}")

    async def tell_waiting(
        self, offset: int, num: int, keys: Optional[List[str]] = None
    ) -> List[StatusInfo]:
        params: list = [offset, num]
        if keys is not None:
            params.append(keys)
        result = await self._call("aria2.tellWaiting", params)
        if isinstance(result, list):
            return [StatusInfo.from_dict(item) for item in result if isinstance(item, dict)]
        raise Aria2Error(f"Unexpected result type for tellWaiting: {type(result)}")

    async def tell_stopped(
        self, offset: int, num: int, keys: Optional[List[str]] = None
    ) -> List[StatusInfo]:
        params: list = [offset, num]
        if keys is not None:
            params.append(keys)
        result = await self._call("aria2.tellStopped", params)
        if isinstance(result, list):
            return [StatusInfo.from_dict(item) for item in result if isinstance(item, dict)]
        raise Aria2Error(f"Unexpected result type for tellStopped: {type(result)}")

    async def get_global_stat(self) -> GlobalStat:
        result = await self._call("aria2.getGlobalStat")
        if isinstance(result, dict):
            return GlobalStat.from_dict(result)
        raise Aria2Error(f"Unexpected result type for getGlobalStat: {type(result)}")

    async def purge_download_result(self) -> str:
        result = await self._call("aria2.purgeDownloadResult")
        return str(result) if result is not None else "OK"

    async def remove_download_result(self, gid: str) -> str:
        result = await self._call("aria2.removeDownloadResult", [gid])
        return str(result) if result is not None else "OK"

    async def get_global_option(self) -> Dict:
        result = await self._call("aria2.getGlobalOption")
        return result if isinstance(result, dict) else {}

    async def change_global_option(self, options: Dict) -> str:
        result = await self._call("aria2.changeGlobalOption", [options])
        return str(result) if result is not None else "OK"

    async def get_option(self, gid: str) -> Dict:
        result = await self._call("aria2.getOption", [gid])
        return result if isinstance(result, dict) else {}

    async def change_option(self, gid: str, options: Dict) -> str:
        result = await self._call("aria2.changeOption", [gid, options])
        return str(result) if result is not None else "OK"

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
        return str(result) if result is not None else "OK"

    async def force_shutdown(self) -> str:
        result = await self._call("aria2.forceShutdown")
        return str(result) if result is not None else "OK"

    async def save_session(self) -> str:
        result = await self._call("aria2.saveSession")
        return str(result) if result is not None else "OK"

    async def subscribe_events(
        self, filter: Optional[List[EventType]] = None
    ) -> EventSubscriber:
        ws_url = _http_to_ws(self._url)
        subscriber = EventSubscriber(ws_url, self._token, filter)
        await subscriber.start()
        return subscriber

    async def close(self) -> None:
        await self._transport.close()
