from __future__ import annotations

import pytest

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.types import DownloadStatus


@pytest.mark.asyncio
class TestHttpDownload:
    async def test_add_uri_and_check_status(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/small.txt"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            gid = await client.add_uri([url])
            assert isinstance(gid, str)
            status = await client.tell_status(gid)
            assert status.gid == gid
            assert status.status in (
                DownloadStatus.ACTIVE.value,
                DownloadStatus.WAITING.value,
                DownloadStatus.COMPLETE.value,
            )

    async def test_tell_status_progress(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/medium.bin"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            gid = await client.add_uri([url])
            status = await client.tell_status(gid)
            assert status.completed_length is not None
            assert status.total_length is not None

    async def test_download_complete_status(self, aria2_server, test_file_server):
        server, tmpdir = test_file_server
        url = f"http://127.0.0.1:{server.port}/small.txt"
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            gid = await client.add_uri([url])
            import asyncio
            for _ in range(20):
                status = await client.tell_status(gid)
                if status.status in (DownloadStatus.COMPLETE.value, DownloadStatus.ERROR.value):
                    break
                await asyncio.sleep(0.5)
            assert status.status == DownloadStatus.COMPLETE.value
