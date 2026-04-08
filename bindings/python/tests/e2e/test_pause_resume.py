from __future__ import annotations

import asyncio

import pytest

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.types import DownloadStatus


@pytest.mark.asyncio
class TestPauseResume:
    async def test_pause_download(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/medium.bin"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            gid = await client.add_uri([url])
            result = await client.pause(gid)
            assert result == gid
            status = await client.tell_status(gid)
            assert status.status in (DownloadStatus.PAUSED.value, DownloadStatus.ACTIVE.value)

    async def test_unpause_download(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/medium.bin"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            gid = await client.add_uri([url])
            await client.pause(gid)
            result = await client.unpause(gid)
            assert result == gid
            status = await client.tell_status(gid)
            assert status.status in (DownloadStatus.ACTIVE.value, DownloadStatus.PAUSED.value)
