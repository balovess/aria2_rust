import asyncio
from unittest.mock import AsyncMock

import pytest

from aria2_rust_client.errors import ConnectionError
from aria2_rust_client.events import EventSubscriber


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
