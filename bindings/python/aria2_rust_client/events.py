from __future__ import annotations

import asyncio
import json
from typing import List, Optional, Self

from .errors import ConnectionError, TimeoutError
from .types import DownloadEvent, EventType

_EVENT_METHOD_MAP: dict[str, EventType] = {
    "aria2.onDownloadStart": EventType.DOWNLOAD_START,
    "aria2.onDownloadPause": EventType.DOWNLOAD_PAUSE,
    "aria2.onDownloadStop": EventType.DOWNLOAD_STOP,
    "aria2.onDownloadComplete": EventType.DOWNLOAD_COMPLETE,
    "aria2.onDownloadError": EventType.DOWNLOAD_ERROR,
    "aria2.onBtDownloadComplete": EventType.BT_DOWNLOAD_COMPLETE,
    "aria2.onBtDownloadError": EventType.BT_DOWNLOAD_ERROR,
}


class EventSubscriber:
    def __init__(
        self,
        ws_url: str,
        token: Optional[str] = None,
        filter: Optional[List[EventType]] = None,
    ) -> None:
        self._ws_url = ws_url
        self._token = token
        self._filter = set(filter) if filter is not None else None
        self._ws: Any = None
        self._listener_task: Optional[asyncio.Task] = None
        self._queue: asyncio.Queue[Optional[DownloadEvent]] = asyncio.Queue()
        self._closed = False
        self._reconnect_attempts = 0
        self._max_reconnect_attempts = 5

    def _should_include(self, event: DownloadEvent) -> bool:
        if self._filter is None:
            return True
        return event.event_type in self._filter

    async def _connect(self) -> None:
        try:
            import websockets

            self._ws = await websockets.connect(self._ws_url)
            self._reconnect_attempts = 0
        except Exception as exc:
            raise ConnectionError(f"Failed to connect WebSocket: {exc}") from exc

    async def _listen(self) -> None:
        while not self._closed:
            if self._ws is None:
                try:
                    await self._connect()
                except ConnectionError:
                    if not await self._try_reconnect():
                        break
                    continue

            try:
                async for raw_message in self._ws:
                    try:
                        message = json.loads(raw_message)
                    except (json.JSONDecodeError, TypeError):
                        continue

                    method = message.get("method", "")
                    if not method.startswith("aria2.on"):
                        continue

                    params = message.get("params", [{}])
                    event_params = params[0] if isinstance(params, list) and params else {}
                    if not isinstance(event_params, dict):
                        event_params = {}

                    event = DownloadEvent.from_rpc_notification(method, event_params)
                    if self._should_include(event):
                        await self._queue.put(event)
            except asyncio.CancelledError:
                break
            except Exception:
                if self._closed:
                    break
                if not await self._try_reconnect():
                    break

        await self._queue.put(None)

    async def _try_reconnect(self) -> bool:
        if self._closed:
            return False
        if self._reconnect_attempts >= self._max_reconnect_attempts:
            return False

        backoff = min(2**self._reconnect_attempts, 16)
        self._reconnect_attempts += 1

        try:
            await asyncio.sleep(backoff)
        except asyncio.CancelledError:
            return False

        try:
            await self._connect()
            return True
        except ConnectionError:
            return await self._try_reconnect()

    async def start(self) -> None:
        await self._connect()
        self._listener_task = asyncio.create_task(self._listen())

    def __aiter__(self) -> Self:
        return self

    async def __anext__(self) -> DownloadEvent:
        if self._closed:
            raise StopAsyncIteration

        event = await self._queue.get()
        if event is None:
            raise StopAsyncIteration
        return event

    async def close(self) -> None:
        self._closed = True

        if self._listener_task is not None:
            self._listener_task.cancel()
            try:
                await self._listener_task
            except asyncio.CancelledError:
                pass
            self._listener_task = None

        if self._ws is not None:
            try:
                await self._ws.close()
            except Exception:
                pass
            self._ws = None

        while not self._queue.empty():
            try:
                self._queue.get_nowait()
            except asyncio.QueueEmpty:
                break
