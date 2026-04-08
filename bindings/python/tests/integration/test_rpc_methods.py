from __future__ import annotations

import pytest

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.errors import RpcError
from aria2_rust_client.types import GlobalStat, SessionInfo, StatusInfo, VersionInfo


@pytest.fixture
async def client(rpc_url):
    c = Aria2Client(url=rpc_url)
    yield c
    await c.close()


@pytest.mark.asyncio
class TestRpcMethods:
    async def test_add_uri_returns_gid(self, client):
        gid = await client.add_uri(["http://example.com/file.zip"])
        assert isinstance(gid, str)
        assert gid.startswith("gid-")

    async def test_add_torrent_returns_gid(self, client):
        gid = await client.add_torrent(b"fake-torrent-data")
        assert isinstance(gid, str)
        assert gid.startswith("gid-")

    async def test_add_metalink_returns_gid(self, client):
        gid = await client.add_metalink(b"<metalink>data</metalink>")
        assert isinstance(gid, str)
        assert gid.startswith("gid-")

    async def test_remove_existing_task(self, client):
        gid = await client.add_uri(["http://example.com/file.zip"])
        result = await client.remove(gid)
        assert result == gid

    async def test_remove_nonexistent_raises_error(self, client):
        with pytest.raises(RpcError):
            await client.remove("nonexistent-gid")

    async def test_pause_and_unpause(self, client):
        gid = await client.add_uri(["http://example.com/file.zip"])
        result = await client.pause(gid)
        assert result == gid

        result = await client.unpause(gid)
        assert result == gid

    async def test_tell_status(self, client):
        gid = await client.add_uri(["http://example.com/file.zip"])
        status = await client.tell_status(gid)
        assert isinstance(status, StatusInfo)
        assert status.gid == gid
        assert status.status == "active"

    async def test_tell_active(self, client):
        await client.add_uri(["http://example.com/file1.zip"])
        await client.add_uri(["http://example.com/file2.zip"])
        active = await client.tell_active()
        assert isinstance(active, list)
        assert len(active) >= 2
        assert all(isinstance(s, StatusInfo) for s in active)

    async def test_tell_waiting(self, client):
        waiting = await client.tell_waiting(0, 10)
        assert isinstance(waiting, list)

    async def test_tell_stopped(self, client):
        stopped = await client.tell_stopped(0, 10)
        assert isinstance(stopped, list)

    async def test_get_global_stat(self, client):
        stat = await client.get_global_stat()
        assert isinstance(stat, GlobalStat)
        assert stat.download_speed == "20480"
        assert stat.num_active is not None

    async def test_get_version(self, client):
        version = await client.get_version()
        assert isinstance(version, VersionInfo)
        assert version.version == "1.37.0"
        assert "Async DNS" in version.enabled_features

    async def test_get_session_info(self, client):
        info = await client.get_session_info()
        assert isinstance(info, SessionInfo)
        assert info.session_id == "test-session-123"

    async def test_shutdown(self, client):
        result = await client.shutdown()
        assert result == "OK"

    async def test_save_session(self, client):
        result = await client.save_session()
        assert result == "OK"

    async def test_purge_download_result(self, client):
        result = await client.purge_download_result()
        assert result == "OK"
