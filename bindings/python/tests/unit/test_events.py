import asyncio
from unittest.mock import AsyncMock

import pytest

from aria2_rust_client.errors import ConnectionError, TimeoutError
from aria2_rust_client.events import EventSubscriber
from aria2_rust_client.types import DownloadEvent, EventType


@pytest.mark.asyncio
async def test_subscriber_async_context_closes_websocket():
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    websocket = AsyncMock()
    subscriber._ws = websocket

    async with subscriber as active:
        assert active is subscriber
        assert not subscriber._closed

    assert subscriber._closed
    websocket.close.assert_awaited_once_with()


@pytest.mark.asyncio
async def test_subscriber_reconnects_after_normal_websocket_close():
    class ClosedWebSocket:
        def __aiter__(self):
            return self

        async def __anext__(self):
            raise StopAsyncIteration

    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    subscriber._ws = ClosedWebSocket()
    subscriber._max_reconnect_attempts = 0

    await asyncio.wait_for(subscriber._listen(), timeout=1)

    assert subscriber._ws is None
    with pytest.raises(StopAsyncIteration):
        await subscriber.__anext__()


@pytest.mark.asyncio
async def test_subscriber_close_wakes_waiting_iterator():
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    next_event = asyncio.create_task(subscriber.__anext__())
    await asyncio.sleep(0)

    await subscriber.close()

    with pytest.raises(StopAsyncIteration):
        await asyncio.wait_for(next_event, timeout=1)


@pytest.mark.asyncio
async def test_subscriber_close_cancels_in_flight_connection(monkeypatch):
    import websockets

    started = asyncio.Event()

    async def blocked_connect(*args, **kwargs):
        started.set()
        await asyncio.Future()

    monkeypatch.setattr(websockets, "connect", blocked_connect)
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    start = asyncio.create_task(subscriber.start())
    await asyncio.wait_for(started.wait(), timeout=1)

    await subscriber.close()

    with pytest.raises(ConnectionError, match="Subscriber has been closed"):
        await asyncio.wait_for(start, timeout=1)


@pytest.mark.asyncio
async def test_subscriber_start_is_idempotent(monkeypatch):
    import websockets

    started = asyncio.Event()

    class BlockingWebSocket:
        def __aiter__(self):
            return self

        async def __anext__(self):
            await asyncio.Future()

        async def close(self):
            pass

    websocket = BlockingWebSocket()

    async def connect(*args, **kwargs):
        started.set()
        return websocket

    monkeypatch.setattr(websockets, "connect", connect)
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    await subscriber.start()
    await asyncio.wait_for(started.wait(), timeout=1)
    listener = subscriber._listener_task

    await subscriber.start()

    assert subscriber._listener_task is listener
    await subscriber.close()


@pytest.mark.asyncio
async def test_subscriber_delivers_events_after_reconnect(monkeypatch):
    import websockets

    class FakeWebSocket:
        def __init__(self, messages):
            self._messages = iter(messages)

        def __aiter__(self):
            return self

        async def __anext__(self):
            try:
                return next(self._messages)
            except StopIteration:
                raise StopAsyncIteration

        async def close(self):
            pass

    first = FakeWebSocket([])
    second = FakeWebSocket(
        [
            '{"method":"aria2.onDownloadComplete",'
            '"params":[{"gid":"reconnected"}]}'
        ]
    )
    connections = iter([first, second])

    async def connect(*args, **kwargs):
        return next(connections)

    monkeypatch.setattr(websockets, "connect", connect)
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    await subscriber.start()

    event = await asyncio.wait_for(subscriber.__anext__(), timeout=3)

    assert event.gid == "reconnected"
    await subscriber.close()


@pytest.mark.asyncio
async def test_subscriber_ignores_non_object_notifications_without_reconnecting():
    class FakeWebSocket:
        def __init__(self):
            self._messages = iter(
                [
                    "[]",
                    "null",
                    "1",
                    '{"method": 42}',
                    '{"method":"aria2.onDownloadComplete",'
                    '"params":[{"gid":"valid"}]}',
                ]
            )

        def __aiter__(self):
            return self

        async def __anext__(self):
            try:
                return next(self._messages)
            except StopIteration:
                raise StopAsyncIteration

    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    subscriber._ws = FakeWebSocket()
    subscriber._max_reconnect_attempts = 0

    await asyncio.wait_for(subscriber._listen(), timeout=1)

    event = await asyncio.wait_for(subscriber.__anext__(), timeout=1)
    assert event.gid == "valid"


@pytest.mark.asyncio
async def test_wait_for_terminal_filters_other_gids_and_non_terminal_events():
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    subscriber.start = AsyncMock()
    waiter = asyncio.create_task(subscriber.wait_for_terminal("target"))
    for _ in range(10):
        if "target" in subscriber._terminal_waiters:
            break
        await asyncio.sleep(0)
    assert "target" in subscriber._terminal_waiters

    subscriber._publish_event(DownloadEvent(EventType.DOWNLOAD_START, gid="other"))
    subscriber._publish_event(DownloadEvent(EventType.DOWNLOAD_COMPLETE, gid="other"))
    expected = DownloadEvent(EventType.DOWNLOAD_ERROR, gid="target", error_code=3)
    subscriber._publish_event(expected)

    assert await waiter == expected
    subscriber.start.assert_awaited_once_with()


@pytest.mark.asyncio
async def test_wait_for_terminal_supports_concurrent_gids():
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    subscriber.start = AsyncMock()
    first_wait = asyncio.create_task(subscriber.wait_for_terminal("first"))
    second_wait = asyncio.create_task(subscriber.wait_for_terminal("second"))

    for _ in range(10):
        if set(subscriber._terminal_waiters) == {"first", "second"}:
            break
        await asyncio.sleep(0)
    assert set(subscriber._terminal_waiters) == {"first", "second"}

    first = DownloadEvent(EventType.DOWNLOAD_COMPLETE, gid="first")
    second = DownloadEvent(EventType.DOWNLOAD_ERROR, gid="second", error_code=3)
    subscriber._publish_event(second)
    subscriber._publish_event(first)

    assert await asyncio.gather(first_wait, second_wait) == [first, second]


@pytest.mark.asyncio
async def test_wait_for_terminal_timeout_uses_sdk_error():
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    subscriber.start = AsyncMock()

    with pytest.raises(TimeoutError, match="target"):
        await subscriber.wait_for_terminal("target", timeout=0.01)


@pytest.mark.asyncio
async def test_wait_for_terminal_rejects_invalid_arguments():
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")

    with pytest.raises(TypeError):
        await subscriber.wait_for_terminal("")
    with pytest.raises(ValueError):
        await subscriber.wait_for_terminal("target", timeout=0)


@pytest.mark.asyncio
async def test_wait_for_terminal_reports_subscriber_close():
    subscriber = EventSubscriber("ws://localhost:6800/jsonrpc")
    subscriber.start = AsyncMock()
    waiter = asyncio.create_task(subscriber.wait_for_terminal("target"))
    await asyncio.sleep(0)

    await subscriber.close()

    with pytest.raises(ConnectionError, match="closed"):
        await asyncio.wait_for(waiter, timeout=1)
