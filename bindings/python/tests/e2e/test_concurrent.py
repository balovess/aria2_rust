from __future__ import annotations

import asyncio

import pytest

from aria2_rust_client.client import Aria2Client


@pytest.mark.asyncio
class TestConcurrent:
    async def test_concurrent_add_uri(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/small.txt"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            tasks = [client.add_uri([url]) for _ in range(10)]
            gids = await asyncio.gather(*tasks)
            assert len(gids) == 10
            assert all(isinstance(gid, str) and len(gid) > 0 for gid in gids)
