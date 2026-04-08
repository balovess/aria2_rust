from __future__ import annotations

import asyncio

import pytest

from aria2_rust_client.client import Aria2Client
from aria2_rust_client.errors import ConnectionError, RpcError, TimeoutError


@pytest.mark.asyncio
class TestErrorHandling:
    async def test_invalid_gid_raises_rpc_error(self, aria2_server):
        async with Aria2Client(url=aria2_server.rpc_url, token="e2e-test-token") as client:
            with pytest.raises(RpcError):
                await client.tell_status("0000000000000000")

    async def test_connection_refused_raises_connection_error(self):
        async with Aria2Client(url="http://127.0.0.1:1/jsonrpc", timeout=2.0) as client:
            with pytest.raises(ConnectionError):
                await client.get_version()

    async def test_timeout_raises_timeout_error(self):
        async def silent_handler(reader, writer):
            await asyncio.sleep(100)
            writer.close()

        server = await asyncio.start_server(silent_handler, "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        try:
            async with Aria2Client(url=f"http://127.0.0.1:{port}/jsonrpc", timeout=0.001) as client:
                with pytest.raises(TimeoutError):
                    await client.get_version()
        finally:
            server.close()
            await server.wait_closed()
