from __future__ import annotations

import pytest
import pytest_asyncio

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.types import StatusInfo


@pytest_asyncio.fixture
async def client(rpc_url):
    c = Aria2Client(url=rpc_url)
    yield c
    await c.close()


class TestMultiTask:
    async def test_add_multiple_tasks(self, client):
        gids = []
        for i in range(5):
            gid = await client.add_uri([f"http://example.com/file{i}.zip"])
            gids.append(gid)
        assert len(gids) == 5
        assert len(set(gids)) == 5

    async def test_tell_active_after_adding(self, client):
        await client.add_uri(["http://example.com/file1.zip"])
        await client.add_uri(["http://example.com/file2.zip"])
        active = await client.tell_active()
        assert len(active) >= 2
        gids = [s.gid for s in active]
        assert len(set(gids)) == len(gids)

    async def test_pause_resume_remove_flow(self, client):
        gid = await client.add_uri(["http://example.com/file.zip"])

        result = await client.pause(gid)
        assert result == gid

        status = await client.tell_status(gid)
        assert status.status == "paused"

        result = await client.unpause(gid)
        assert result == gid

        status = await client.tell_status(gid)
        assert status.status == "active"

        result = await client.remove(gid)
        assert result == gid

        status = await client.tell_status(gid)
        assert status.status == "removed"
