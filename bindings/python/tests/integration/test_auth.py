from __future__ import annotations

import pytest
import pytest_asyncio

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.errors import AuthError


class TestAuth:
    async def test_token_auth_success(self, rpc_url_with_token, rpc_server_with_token):
        async with Aria2Client(url=rpc_url_with_token, token="test-token") as client:
            gid = await client.add_uri(["http://example.com/file.zip"])
            assert isinstance(gid, str)

    async def test_token_auth_failure(self, rpc_url_with_token, rpc_server_with_token):
        async with Aria2Client(url=rpc_url_with_token, token="wrong-token") as client:
            with pytest.raises(AuthError):
                await client.add_uri(["http://example.com/file.zip"])

    async def test_no_auth_when_no_token_configured(self, rpc_url, rpc_server):
        async with Aria2Client(url=rpc_url) as client:
            gid = await client.add_uri(["http://example.com/file.zip"])
            assert isinstance(gid, str)
