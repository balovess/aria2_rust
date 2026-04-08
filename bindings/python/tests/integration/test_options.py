from __future__ import annotations

import pytest

from aria2_rust_client.client import Aria2Client


@pytest.fixture
async def client(rpc_url):
    c = Aria2Client(url=rpc_url)
    yield c
    await c.close()


@pytest.mark.asyncio
class TestOptions:
    async def test_get_global_option(self, client):
        options = await client.get_global_option()
        assert isinstance(options, dict)
        assert "max-concurrent-downloads" in options

    async def test_change_global_option(self, client):
        result = await client.change_global_option({"max-concurrent-downloads": "10"})
        assert result == "OK"

    async def test_get_option_for_task(self, client):
        gid = await client.add_uri(["http://example.com/file.zip"])
        options = await client.get_option(gid)
        assert isinstance(options, dict)
        assert "dir" in options

    async def test_change_option_for_task(self, client):
        gid = await client.add_uri(["http://example.com/file.zip"])
        result = await client.change_option(gid, {"max-download-limit": "100K"})
        assert result == "OK"
