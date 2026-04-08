from __future__ import annotations

import asyncio
import json
import uuid
from typing import Any, Dict, List, Optional

import pytest


class MockAria2Server:
    def __init__(self, token: Optional[str] = None):
        self.token = token
        self._tasks: Dict[str, Dict[str, Any]] = {}
        self._server: Optional[asyncio.Server] = None
        self.port: int = 0
        self._gid_counter = 0

    def _next_gid(self) -> str:
        self._gid_counter += 1
        return f"gid-{self._gid_counter:08d}"

    def _make_status(self, gid: str, status: str = "active") -> Dict[str, Any]:
        return {
            "gid": gid,
            "totalLength": "1048576",
            "completedLength": "512000",
            "uploadLength": "0",
            "downloadSpeed": "1024",
            "uploadSpeed": "0",
            "errorCode": "0",
            "status": status,
            "dir": "/downloads",
            "files": [
                {
                    "index": "1",
                    "path": "/downloads/file.zip",
                    "length": "1048576",
                    "completedLength": "512000",
                    "selected": "true",
                    "uris": [{"uri": "http://example.com/file.zip", "status": "used"}],
                }
            ],
        }

    def _verify_token(self, params: list) -> Optional[Dict[str, Any]]:
        if self.token is None:
            return None
        if not params or not isinstance(params[0], str) or not params[0].startswith("token:"):
            return {"code": 1, "message": "Unauthorized"}
        if params[0] != f"token:{self.token}":
            return {"code": 1, "message": "Unauthorized"}
        return None

    def _get_params(self, params: list) -> list:
        if self.token is not None and params and isinstance(params[0], str) and params[0].startswith("token:"):
            return params[1:]
        return params

    def _handle_method(self, method: str, params: list) -> Any:
        auth_err = self._verify_token(params)
        if auth_err is not None:
            return None, auth_err

        params = self._get_params(params)

        if method == "aria2.addUri":
            gid = self._next_gid()
            self._tasks[gid] = {"status": "active", "data": self._make_status(gid, "active")}
            return gid, None

        elif method == "aria2.addTorrent":
            gid = self._next_gid()
            self._tasks[gid] = {"status": "active", "data": self._make_status(gid, "active")}
            return gid, None

        elif method == "aria2.addMetalink":
            gid = self._next_gid()
            self._tasks[gid] = {"status": "active", "data": self._make_status(gid, "active")}
            return gid, None

        elif method == "aria2.remove":
            gid = params[0] if params else ""
            if gid in self._tasks:
                self._tasks[gid]["status"] = "removed"
                self._tasks[gid]["data"]["status"] = "removed"
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.pause":
            gid = params[0] if params else ""
            if gid in self._tasks:
                self._tasks[gid]["status"] = "paused"
                self._tasks[gid]["data"]["status"] = "paused"
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.unpause":
            gid = params[0] if params else ""
            if gid in self._tasks:
                self._tasks[gid]["status"] = "active"
                self._tasks[gid]["data"]["status"] = "active"
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.forcePause":
            gid = params[0] if params else ""
            if gid in self._tasks:
                self._tasks[gid]["status"] = "paused"
                self._tasks[gid]["data"]["status"] = "paused"
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.forceRemove":
            gid = params[0] if params else ""
            if gid in self._tasks:
                self._tasks[gid]["status"] = "removed"
                self._tasks[gid]["data"]["status"] = "removed"
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.forceUnpause":
            gid = params[0] if params else ""
            if gid in self._tasks:
                self._tasks[gid]["status"] = "active"
                self._tasks[gid]["data"]["status"] = "active"
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.tellStatus":
            gid = params[0] if params else ""
            if gid in self._tasks:
                return self._tasks[gid]["data"], None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.tellActive":
            result = [t["data"] for t in self._tasks.values() if t["status"] == "active"]
            return result, None

        elif method == "aria2.tellWaiting":
            result = [t["data"] for t in self._tasks.values() if t["status"] == "waiting"]
            return result, None

        elif method == "aria2.tellStopped":
            result = [t["data"] for t in self._tasks.values() if t["status"] in ("complete", "removed", "error")]
            return result, None

        elif method == "aria2.getGlobalStat":
            active = sum(1 for t in self._tasks.values() if t["status"] == "active")
            waiting = sum(1 for t in self._tasks.values() if t["status"] == "waiting")
            stopped = sum(1 for t in self._tasks.values() if t["status"] in ("complete", "removed", "error"))
            return {
                "downloadSpeed": "20480",
                "uploadSpeed": "0",
                "numActive": str(active),
                "numWaiting": str(waiting),
                "numStopped": str(stopped),
                "numStoppedTotal": str(stopped),
            }, None

        elif method == "aria2.getVersion":
            return {
                "version": "1.37.0",
                "enabledFeatures": ["Async DNS", "BitTorrent"],
            }, None

        elif method == "aria2.getSessionInfo":
            return {"sessionId": "test-session-123"}, None

        elif method == "aria2.purgeDownloadResult":
            to_remove = [g for g, t in self._tasks.items() if t["status"] in ("complete", "removed", "error")]
            for g in to_remove:
                del self._tasks[g]
            return "OK", None

        elif method == "aria2.removeDownloadResult":
            gid = params[0] if params else ""
            if gid in self._tasks and self._tasks[gid]["status"] in ("complete", "removed", "error"):
                del self._tasks[gid]
                return "OK", None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.getGlobalOption":
            return {"max-concurrent-downloads": "5", "max-overall-download-limit": "0"}, None

        elif method == "aria2.changeGlobalOption":
            return "OK", None

        elif method == "aria2.getOption":
            return {"dir": "/downloads", "max-download-limit": "0"}, None

        elif method == "aria2.changeOption":
            return "OK", None

        elif method == "aria2.shutdown":
            return "OK", None

        elif method == "aria2.forceShutdown":
            return "OK", None

        elif method == "aria2.saveSession":
            return "OK", None

        return None, {"code": -1, "message": f"Unknown method: {method}"}

    async def _handle_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        try:
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                try:
                    request = json.loads(data.decode("utf-8"))
                except (json.JSONDecodeError, UnicodeDecodeError):
                    response = {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "Parse error"}}
                    writer.write(json.dumps(response).encode("utf-8"))
                    await writer.drain()
                    continue

                req_id = request.get("id")
                method = request.get("method", "")
                params = request.get("params", [])

                result, error = self._handle_method(method, params)

                response: Dict[str, Any] = {"jsonrpc": "2.0", "id": req_id}
                if error is not None:
                    response["error"] = error
                else:
                    response["result"] = result

                writer.write(json.dumps(response).encode("utf-8"))
                await writer.drain()
        except (ConnectionResetError, asyncio.IncompleteReadError):
            pass
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

    async def start(self):
        self._server = await asyncio.start_server(
            self._handle_client, "127.0.0.1", 0
        )
        addrs = self._server.sockets[0].getsockname()
        self.port = addrs[1]

    async def stop(self):
        if self._server is not None:
            self._server.close()
            await self._server.wait_closed()
            self._server = None


@pytest.fixture
async def rpc_server():
    server = MockAria2Server()
    await server.start()
    yield server
    await server.stop()


@pytest.fixture
async def rpc_server_with_token():
    server = MockAria2Server(token="test-token")
    await server.start()
    yield server
    await server.stop()


@pytest.fixture
def rpc_url(rpc_server):
    return f"http://127.0.0.1:{rpc_server.port}/jsonrpc"


@pytest.fixture
def rpc_url_with_token(rpc_server_with_token):
    return f"http://127.0.0.1:{rpc_server_with_token.port}/jsonrpc"
