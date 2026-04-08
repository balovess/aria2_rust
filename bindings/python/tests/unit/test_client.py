from __future__ import annotations

import base64
from typing import Any, List
from unittest.mock import AsyncMock

import pytest

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.errors import Aria2Error
from aria2_rust_client.types import GlobalStat, SessionInfo, StatusInfo, VersionInfo


class MockTransport:
    def __init__(self):
        self.send_request = AsyncMock()
        self.close = AsyncMock()

    async def send_request(self, method: str, params: list) -> Any:
        return await self.send_request(method, params)

    async def close(self) -> None:
        await self.close()


@pytest.fixture
def mock_transport():
    return MockTransport()


@pytest.fixture
def client(mock_transport):
    c = Aria2Client.__new__(Aria2Client)
    c._url = "http://localhost:6800/jsonrpc"
    c._token = None
    c._timeout = 30.0
    c._transport = mock_transport
    return c


class TestAddUri:
    @pytest.mark.asyncio
    async def test_sends_correct_method_and_params(self, client, mock_transport):
        mock_transport.send_request.return_value = "2089b05ecca3d829"
        result = await client.add_uri(["http://example.com/file.zip"])
        mock_transport.send_request.assert_called_once_with(
            "aria2.addUri", [["http://example.com/file.zip"]]
        )
        assert result == "2089b05ecca3d829"

    @pytest.mark.asyncio
    async def test_with_options(self, client, mock_transport):
        mock_transport.send_request.return_value = "gid1"
        result = await client.add_uri(
            ["http://example.com/file.zip"], {"dir": "/downloads"}
        )
        mock_transport.send_request.assert_called_once_with(
            "aria2.addUri", [["http://example.com/file.zip"], {"dir": "/downloads"}]
        )
        assert result == "gid1"


class TestAddTorrent:
    @pytest.mark.asyncio
    async def test_base64_encodes_torrent(self, client, mock_transport):
        torrent_data = b"fake-torrent-content"
        mock_transport.send_request.return_value = "torrent-gid"
        result = await client.add_torrent(torrent_data)
        expected_encoded = base64.b64encode(torrent_data).decode("ascii")
        mock_transport.send_request.assert_called_once_with(
            "aria2.addTorrent", [expected_encoded]
        )
        assert result == "torrent-gid"

    @pytest.mark.asyncio
    async def test_with_options(self, client, mock_transport):
        torrent_data = b"data"
        mock_transport.send_request.return_value = "gid"
        await client.add_torrent(torrent_data, {"dir": "/tmp"})
        expected_encoded = base64.b64encode(torrent_data).decode("ascii")
        mock_transport.send_request.assert_called_once_with(
            "aria2.addTorrent", [expected_encoded, {"dir": "/tmp"}]
        )


class TestAddMetalink:
    @pytest.mark.asyncio
    async def test_base64_encodes_metalink(self, client, mock_transport):
        metalink_data = b"<metalink>content</metalink>"
        mock_transport.send_request.return_value = "metalink-gid"
        result = await client.add_metalink(metalink_data)
        expected_encoded = base64.b64encode(metalink_data).decode("ascii")
        mock_transport.send_request.assert_called_once_with(
            "aria2.addMetalink", [expected_encoded]
        )
        assert result == "metalink-gid"


class TestSimpleMethods:
    @pytest.mark.asyncio
    async def test_remove(self, client, mock_transport):
        mock_transport.send_request.return_value = "gid1"
        result = await client.remove("gid1")
        mock_transport.send_request.assert_called_once_with("aria2.remove", ["gid1"])
        assert result == "gid1"

    @pytest.mark.asyncio
    async def test_pause(self, client, mock_transport):
        mock_transport.send_request.return_value = "gid1"
        result = await client.pause("gid1")
        mock_transport.send_request.assert_called_once_with("aria2.pause", ["gid1"])
        assert result == "gid1"

    @pytest.mark.asyncio
    async def test_unpause(self, client, mock_transport):
        mock_transport.send_request.return_value = "gid1"
        result = await client.unpause("gid1")
        mock_transport.send_request.assert_called_once_with("aria2.unpause", ["gid1"])
        assert result == "gid1"

    @pytest.mark.asyncio
    async def test_force_pause(self, client, mock_transport):
        mock_transport.send_request.return_value = "gid1"
        result = await client.force_pause("gid1")
        mock_transport.send_request.assert_called_once_with("aria2.forcePause", ["gid1"])
        assert result == "gid1"

    @pytest.mark.asyncio
    async def test_force_remove(self, client, mock_transport):
        mock_transport.send_request.return_value = "gid1"
        result = await client.force_remove("gid1")
        mock_transport.send_request.assert_called_once_with("aria2.forceRemove", ["gid1"])
        assert result == "gid1"

    @pytest.mark.asyncio
    async def test_force_unpause(self, client, mock_transport):
        mock_transport.send_request.return_value = "gid1"
        result = await client.force_unpause("gid1")
        mock_transport.send_request.assert_called_once_with("aria2.forceUnpause", ["gid1"])
        assert result == "gid1"


class TestTellStatus:
    @pytest.mark.asyncio
    async def test_returns_status_info(self, client, mock_transport):
        mock_transport.send_request.return_value = {
            "gid": "2089b05ecca3d829",
            "status": "complete",
            "totalLength": "34896136",
            "completedLength": "34896136",
        }
        result = await client.tell_status("2089b05ecca3d829")
        assert isinstance(result, StatusInfo)
        assert result.gid == "2089b05ecca3d829"
        assert result.status == "complete"

    @pytest.mark.asyncio
    async def test_with_keys(self, client, mock_transport):
        mock_transport.send_request.return_value = {"gid": "abc", "status": "active"}
        result = await client.tell_status("abc", keys=["gid", "status"])
        mock_transport.send_request.assert_called_once_with(
            "aria2.tellStatus", ["abc", ["gid", "status"]]
        )

    @pytest.mark.asyncio
    async def test_raises_on_non_dict_result(self, client, mock_transport):
        mock_transport.send_request.return_value = "not-a-dict"
        with pytest.raises(Aria2Error):
            await client.tell_status("abc")


class TestTellLists:
    @pytest.mark.asyncio
    async def test_tell_active(self, client, mock_transport):
        mock_transport.send_request.return_value = [
            {"gid": "gid1", "status": "active"},
            {"gid": "gid2", "status": "active"},
        ]
        result = await client.tell_active()
        assert len(result) == 2
        assert all(isinstance(r, StatusInfo) for r in result)
        assert result[0].gid == "gid1"

    @pytest.mark.asyncio
    async def test_tell_waiting(self, client, mock_transport):
        mock_transport.send_request.return_value = [
            {"gid": "gid3", "status": "waiting"},
        ]
        result = await client.tell_waiting(0, 10)
        mock_transport.send_request.assert_called_once_with(
            "aria2.tellWaiting", [0, 10]
        )
        assert len(result) == 1

    @pytest.mark.asyncio
    async def test_tell_stopped(self, client, mock_transport):
        mock_transport.send_request.return_value = [
            {"gid": "gid4", "status": "complete"},
        ]
        result = await client.tell_stopped(0, 10)
        mock_transport.send_request.assert_called_once_with(
            "aria2.tellStopped", [0, 10]
        )
        assert len(result) == 1

    @pytest.mark.asyncio
    async def test_tell_active_raises_on_non_list(self, client, mock_transport):
        mock_transport.send_request.return_value = "not-a-list"
        with pytest.raises(Aria2Error):
            await client.tell_active()


class TestGetGlobalStat:
    @pytest.mark.asyncio
    async def test_returns_global_stat(self, client, mock_transport):
        mock_transport.send_request.return_value = {
            "downloadSpeed": "20480",
            "uploadSpeed": "0",
            "numActive": "1",
            "numWaiting": "2",
            "numStopped": "3",
            "numStoppedTotal": "5",
        }
        result = await client.get_global_stat()
        assert isinstance(result, GlobalStat)
        assert result.download_speed == "20480"
        assert result.num_active == "1"


class TestGetVersion:
    @pytest.mark.asyncio
    async def test_returns_version_info(self, client, mock_transport):
        mock_transport.send_request.return_value = {
            "version": "1.37.0",
            "enabledFeatures": ["Async DNS", "BitTorrent"],
        }
        result = await client.get_version()
        assert isinstance(result, VersionInfo)
        assert result.version == "1.37.0"
        assert "Async DNS" in result.enabled_features


class TestGetSessionInfo:
    @pytest.mark.asyncio
    async def test_returns_session_info(self, client, mock_transport):
        mock_transport.send_request.return_value = {"sessionId": "abc123"}
        result = await client.get_session_info()
        assert isinstance(result, SessionInfo)
        assert result.session_id == "abc123"


class TestOptions:
    @pytest.mark.asyncio
    async def test_get_global_option(self, client, mock_transport):
        mock_transport.send_request.return_value = {"max-concurrent-downloads": "5"}
        result = await client.get_global_option()
        assert result == {"max-concurrent-downloads": "5"}

    @pytest.mark.asyncio
    async def test_change_global_option(self, client, mock_transport):
        mock_transport.send_request.return_value = "OK"
        result = await client.change_global_option({"max-concurrent-downloads": "10"})
        mock_transport.send_request.assert_called_once_with(
            "aria2.changeGlobalOption", [{"max-concurrent-downloads": "10"}]
        )
        assert result == "OK"

    @pytest.mark.asyncio
    async def test_get_option(self, client, mock_transport):
        mock_transport.send_request.return_value = {"dir": "/downloads"}
        result = await client.get_option("gid1")
        mock_transport.send_request.assert_called_once_with("aria2.getOption", ["gid1"])
        assert result == {"dir": "/downloads"}

    @pytest.mark.asyncio
    async def test_change_option(self, client, mock_transport):
        mock_transport.send_request.return_value = "OK"
        result = await client.change_option("gid1", {"dir": "/tmp"})
        mock_transport.send_request.assert_called_once_with(
            "aria2.changeOption", ["gid1", {"dir": "/tmp"}]
        )
        assert result == "OK"


class TestPurgeAndRemoveResult:
    @pytest.mark.asyncio
    async def test_purge_download_result(self, client, mock_transport):
        mock_transport.send_request.return_value = "OK"
        result = await client.purge_download_result()
        mock_transport.send_request.assert_called_once_with(
            "aria2.purgeDownloadResult", []
        )
        assert result == "OK"

    @pytest.mark.asyncio
    async def test_remove_download_result(self, client, mock_transport):
        mock_transport.send_request.return_value = "OK"
        result = await client.remove_download_result("gid1")
        mock_transport.send_request.assert_called_once_with(
            "aria2.removeDownloadResult", ["gid1"]
        )
        assert result == "OK"


class TestShutdown:
    @pytest.mark.asyncio
    async def test_shutdown(self, client, mock_transport):
        mock_transport.send_request.return_value = "OK"
        result = await client.shutdown()
        mock_transport.send_request.assert_called_once_with("aria2.shutdown", [])
        assert result == "OK"

    @pytest.mark.asyncio
    async def test_force_shutdown(self, client, mock_transport):
        mock_transport.send_request.return_value = "OK"
        result = await client.force_shutdown()
        mock_transport.send_request.assert_called_once_with("aria2.forceShutdown", [])
        assert result == "OK"

    @pytest.mark.asyncio
    async def test_save_session(self, client, mock_transport):
        mock_transport.send_request.return_value = "OK"
        result = await client.save_session()
        mock_transport.send_request.assert_called_once_with("aria2.saveSession", [])
        assert result == "OK"


class TestContextManager:
    @pytest.mark.asyncio
    async def test_async_context_manager(self, client, mock_transport):
        async with client as c:
            assert c is client
        mock_transport.close.assert_called_once()

    @pytest.mark.asyncio
    async def test_close(self, client, mock_transport):
        await client.close()
        mock_transport.close.assert_called_once()
