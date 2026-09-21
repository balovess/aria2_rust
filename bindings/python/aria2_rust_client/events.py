from __future__ import annotations

import asyncio
import json
from typing import Any, List, Optional

try:
    from typing import Self
except ImportError:
    from typing_extensions import Self

from .errors import ConnectionError, TimeoutError
from .types import DownloadEvent, EventType

_EVENT_METHOD_MAP: dict[str, EventType] = {
    "aria2.onDownloadStart": EventType.DOWNLOAD_START,
    "aria2.onDownloadPause": EventType.DOWNLOAD_PAUSE,
    "aria2.onDownloadStop": EventType.DOWNLOAD_STOP,
    "aria2.onDownloadComplete": EventType.DOWNLOAD_COMPLETE,
    "aria2.onDownloadError": EventType.DOWNLOAD_ERROR,
    "aria2.onBtDownloadComplete": EventType.BT_DOWNLOAD_COMPLETE,
}

_TERMINAL_EVENT_TYPES = frozenset(
    {
        EventType.DOWNLOAD_STOP,
        EventType.DOWNLOAD_COMPLETE,
        EventType.DOWNLOAD_ERROR,
        EventType.BT_DOWNLOAD_COMPLETE,
    }
)


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
        self._connect_task: Optional[asyncio.Task] = None
        self._queue: asyncio.Queue[Optional[DownloadEvent]] = asyncio.Queue()
        self._terminal_waiters: dict[
            str, set[asyncio.Future[DownloadEvent]]
        ] = {}
        self._closed = False
        self._reconnect_attempts = 0
        self._max_reconnect_attempts = 5

    def _should_include(self, event: DownloadEvent) -> bool:
        if self._filter is None:
            return True
        return event.event_type in self._filter

    def _publish_event(self, event: DownloadEvent) -> None:
        """Fan out one accepted event to iteration and GID-specific waiters."""
        self._queue.put_nowait(event)
        if event.event_type not in _TERMINAL_EVENT_TYPES:
            return

        waiters = self._terminal_waiters.pop(event.gid, ())
        for waiter in waiters:
            if not waiter.done():
                waiter.set_result(event)

    def _reject_terminal_waiters(self, error: ConnectionError) -> None:
        waiters = self._terminal_waiters
        self._terminal_waiters = {}
        for pending in waiters.values():
            for waiter in pending:
                if not waiter.done():
                    waiter.set_exception(error)

    async def _connect(self) -> None:
        if self._closed:
            raise ConnectionError("Subscriber has been closed")

        task = self._connect_task
        if task is None:
            task = asyncio.create_task(self._open_connection())
            self._connect_task = task

        try:
            await asyncio.shield(task)
        except asyncio.CancelledError as exc:
            if self._closed:
                raise ConnectionError("Subscriber has been closed") from exc
            raise
        finally:
            if self._connect_task is task:
                self._connect_task = None

    async def _open_connection(self) -> None:
        try:
            import websockets

            ws = await websockets.connect(self._ws_url)
            if self._closed:
                await ws.close()
                raise ConnectionError("Subscriber has been closed")
            self._ws = ws
            self._reconnect_attempts = 0
        except ConnectionError:
            raise
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

            connection = self._ws
            if connection is None:
                continue

            try:
                async for raw_message in connection:
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
                    if event is not None and self._should_include(event):
                        self._publish_event(event)
            except asyncio.CancelledError:
                break
            except Exception:
                pass

            if self._ws is connection:
                self._ws = None
            if self._closed or not await self._try_reconnect():
                break

        self._reject_terminal_waiters(
            ConnectionError("Subscriber closed before a terminal event was received")
        )
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
        if self._closed:
            raise ConnectionError("Subscriber has been closed")
        if self._listener_task is not None and not self._listener_task.done():
            return

        await self._connect()
        if self._closed:
            raise ConnectionError("Subscriber has been closed")
        if self._listener_task is not None and not self._listener_task.done():
            return
        self._listener_task = asyncio.create_task(self._listen())

    async def __aenter__(self) -> Self:
        """Return the active subscriber for use with ``async with``."""
        return self

    async def __aexit__(self, *args: Any) -> None:
        """Close the WebSocket and listener task when leaving the context."""
        await self.close()

    def __aiter__(self) -> Self:
        return self

    async def __anext__(self) -> DownloadEvent:
        if self._closed:
            raise StopAsyncIteration

        event = await self._queue.get()
        if event is None:
            raise StopAsyncIteration
        return event

    async def wait_for_terminal(
        self, gid: str, timeout: Optional[float] = None
    ) -> DownloadEvent:
        """Wait for a terminal event for one GID without polling status.

        The subscriber is started automatically when needed. Events for other
        GIDs and non-terminal transitions remain available to async iteration,
        while terminal events are independently routed to matching waiters.
        Callers should create the subscriber before submitting a task when
        they must not miss a fast completion event.

        ``timeout`` is optional because a download may legitimately take an
        unbounded amount of time. When supplied, it must be positive and a
        timeout is reported through the SDK's :class:`TimeoutError`.
        """
        if not isinstance(gid, str) or not gid:
            raise TypeError("gid must be a non-empty string")
        if timeout is not None and (
            isinstance(timeout, bool) or not isinstance(timeout, (int, float))
        ):
            raise TypeError("timeout must be a positive number or None")
        if timeout is not None and timeout <= 0:
            raise ValueError("timeout must be positive")

        await self.start()

        loop = asyncio.get_running_loop()
        waiter = loop.create_future()
        self._terminal_waiters.setdefault(gid, set()).add(waiter)

        try:
            if timeout is None:
                return await waiter
            return await asyncio.wait_for(waiter, timeout=timeout)
        except asyncio.TimeoutError as exc:
            raise TimeoutError(
                f"Timed out waiting for terminal event for GID {gid}"
            ) from exc
        finally:
            waiters = self._terminal_waiters.get(gid)
            if waiters is not None:
                waiters.discard(waiter)
                if not waiters:
                    self._terminal_waiters.pop(gid, None)

    async def close(self) -> None:
        self._closed = True

        if self._connect_task is not None:
            self._connect_task.cancel()
            try:
                await self._connect_task
            except asyncio.CancelledError:
                pass
            self._connect_task = None

        if self._listener_task is not None:
            self._listener_task.cancel()
            try:
                await self._listener_task
            except asyncio.CancelledError:
                pass
            self._listener_task = None

        self._reject_terminal_waiters(ConnectionError("Subscriber has been closed"))

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
        await self._queue.put(None)
