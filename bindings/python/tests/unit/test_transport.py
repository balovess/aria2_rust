from __future__ import annotations

import json

import httpx
import pytest
import respx

from aria2_rust_client.errors import AuthError, ConnectionError, RpcError, TimeoutError
from aria2_rust_client.transport import HttpTransport


@pytest.fixture
def transport():
    return HttpTransport("http://localhost:6800/jsonrpc")


@pytest.fixture
def transport_with_token():
    return HttpTransport("http://localhost:6800/jsonrpc", token="secret123")


class TestBuildRequest:
    def test_jsonrpc_2_0_structure(self, transport):
        req = transport._build_request("aria2.addUri", [["http://example.com"]])
        assert req["jsonrpc"] == "2.0"
        assert req["method"] == "aria2.addUri"
        assert req["params"] == [["http://example.com"]]
        assert "id" in req

    def test_auto_increment_id(self, transport):
        id1 = transport._next_id()
        id2 = transport._next_id()
        id3 = transport._next_id()
        assert id1 < id2 < id3

    def test_request_id_increments_on_build(self, transport):
        req1 = transport._build_request("aria2.getVersion", [])
        req2 = transport._build_request("aria2.getVersion", [])
        assert req1["id"] < req2["id"]

    def test_token_prepended_as_token_prefix(self, transport_with_token):
        req = transport_with_token._build_request("aria2.addUri", [["http://x"]])
        assert req["params"][0] == "token:secret123"
        assert req["params"][1] == ["http://x"]

    def test_no_token_when_not_configured(self, transport):
        req = transport._build_request("aria2.getVersion", [])
        assert req["params"] == []


class TestSendRequest:
    @respx.mock
    @pytest.mark.asyncio
    async def test_successful_request(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(
                200,
                json={"jsonrpc": "2.0", "id": 1, "result": "2089b05ecca3d829"},
            )
        )
        result = await transport.send_request("aria2.addUri", [["http://x"]])
        assert result == "2089b05ecca3d829"

    @respx.mock
    @pytest.mark.asyncio
    async def test_rpc_error_response(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(
                200,
                json={
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {"code": 1, "message": "Unauthorized"},
                },
            )
        )
        with pytest.raises(AuthError) as exc_info:
            await transport.send_request("aria2.getVersion", [])
        assert "Unauthorized" in str(exc_info.value)

    @respx.mock
    @pytest.mark.asyncio
    async def test_rpc_generic_error(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(
                200,
                json={
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {"code": 3, "message": "Resource not found"},
                },
            )
        )
        with pytest.raises(RpcError) as exc_info:
            await transport.send_request("aria2.remove", ["bad-gid"])
        assert exc_info.value.code == 3
        assert "Resource not found" in str(exc_info.value)

    @respx.mock
    @pytest.mark.asyncio
    async def test_connection_error_on_network_failure(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            side_effect=httpx.ConnectError("Connection refused")
        )
        with pytest.raises(ConnectionError):
            await transport.send_request("aria2.getVersion", [])

    @respx.mock
    @pytest.mark.asyncio
    async def test_timeout_error(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            side_effect=httpx.ReadTimeout("Read timed out")
        )
        with pytest.raises(TimeoutError):
            await transport.send_request("aria2.getVersion", [])

    @respx.mock
    @pytest.mark.asyncio
    async def test_auth_error_on_401(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(401, text="Unauthorized")
        )
        with pytest.raises(AuthError):
            await transport.send_request("aria2.getVersion", [])

    @respx.mock
    @pytest.mark.asyncio
    async def test_auth_error_on_403(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(403, text="Forbidden")
        )
        with pytest.raises(AuthError):
            await transport.send_request("aria2.getVersion", [])

    @respx.mock
    @pytest.mark.asyncio
    async def test_connection_error_on_500(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(500, text="Internal Server Error")
        )
        with pytest.raises(ConnectionError):
            await transport.send_request("aria2.getVersion", [])

    @respx.mock
    @pytest.mark.asyncio
    async def test_auth_error_code_1_in_rpc_error(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(
                200,
                json={"jsonrpc": "2.0", "id": 1, "error": {"code": 1, "message": "Auth fail"}},
            )
        )
        with pytest.raises(AuthError):
            await transport.send_request("aria2.getVersion", [])

    @respx.mock
    @pytest.mark.asyncio
    async def test_auth_error_code_2_in_rpc_error(self, transport):
        respx.post("http://localhost:6800/jsonrpc").mock(
            return_value=httpx.Response(
                200,
                json={"jsonrpc": "2.0", "id": 1, "error": {"code": 2, "message": "Auth fail"}},
            )
        )
        with pytest.raises(AuthError):
            await transport.send_request("aria2.getVersion", [])

    @pytest.mark.asyncio
    async def test_close(self, transport):
        await transport.close()
