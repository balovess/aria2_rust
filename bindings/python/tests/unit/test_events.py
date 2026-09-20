from unittest.mock import AsyncMock

import pytest

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
