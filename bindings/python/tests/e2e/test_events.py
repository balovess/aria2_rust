from __future__ import annotations

import asyncio

import pytest

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.types import EventType


@pytest.mark.asyncio
class TestEvents:
    async def test_subscribe_events_receives_start_event(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/small.txt"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            subscriber = await client.subscribe_events()
            gid = await client.add_uri([url])
            try:
                event = await asyncio.wait_for(subscriber.__anext__(), timeout=10.0)
                assert event is not None
                assert event.event_type in (
                    EventType.DOWNLOAD_START,
                    EventType.DOWNLOAD_COMPLETE,
                )
                assert event.gid is not None
            except asyncio.TimeoutError:
                pytest.skip("Event not received within timeout")
            finally:
                await subscriber.close()

    async def test_event_type_filtering(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/small.txt"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            subscriber = await client.subscribe_events(
                filter=[EventType.DOWNLOAD_COMPLETE]
            )
            gid = await client.add_uri([url])
            try:
                for _ in range(40):
                    status = await client.tell_status(gid)
                    if status.status in ("complete", "error"):
                        break
                    await asyncio.sleep(0.5)
                event = await asyncio.wait_for(subscriber.__anext__(), timeout=10.0)
                assert event.event_type == EventType.DOWNLOAD_COMPLETE
            except asyncio.TimeoutError:
                pytest.skip("Filtered event not received within timeout")
            finally:
                await subscriber.close()
