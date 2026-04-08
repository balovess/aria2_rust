from __future__ import annotations

import asyncio
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from threading import Thread
from typing import Any, Dict, List, Optional
import socket

import pytest
import pytest_asyncio


class MockAria2Server:
    def __init__(self, token: Optional[str] = None):
        self.token = token
        self._tasks: Dict[str, Dict[str, Any]] = {}
        self._server: Optional[HTTPServer] = None
        self.port: int = 0
        self._gid_counter = 0
        self._thread: Optional[Thread] = None

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
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.forceRemove":
            gid = params[0] if params else ""
            if gid in self._tasks:
                del self._tasks[gid]
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.forceUnpause":
            gid = params[0] if params else ""
            if gid in self._tasks:
                self._tasks[gid]["status"] = "active"
                return gid, None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.tellStatus":
            gid = params[0] if params else ""
            if gid in self._tasks:
                return self._tasks[gid]["data"], None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.tellActive":
            return [t["data"] for t in self._tasks.values() if t["status"] == "active"], None

        elif method == "aria2.tellWaiting":
            return [t["data"] for t in self._tasks.values() if t["status"] == "waiting"], None

        elif method == "aria2.tellStopped":
            return [t["data"] for t in self._tasks.values() if t["status"] in ["stopped", "removed", "complete"]], None

        elif method == "aria2.getGlobalStat":
            return {
                "downloadSpeed": "1024",
                "uploadSpeed": "0",
                "numActive": str(len([t for t in self._tasks.values() if t["status"] == "active"])),
                "numWaiting": "0",
                "numStopped": str(len([t for t in self._tasks.values() if t["status"] != "active"])),
                "numStoppedTotal": "0",
            }, None

        elif method == "aria2.purgeDownloadResult":
            self._tasks.clear()
            return "OK", None

        elif method == "aria2.removeDownloadResult":
            gid = params[0] if params else ""
            if gid in self._tasks:
                del self._tasks[gid]
                return "OK", None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.getGlobalOption":
            return {"max-download-limit": "1048576"}, None

        elif method == "aria2.changeGlobalOption":
            return "OK", None

        elif method == "aria2.getOption":
            return {"max-download-limit": "1048576"}, None

        elif method == "aria2.changeOption":
            gid = params[0] if params else ""
            if gid in self._tasks:
                return "OK", None
            return None, {"code": 1, "message": f"GID {gid} not found"}

        elif method == "aria2.getVersion":
            return {"version": "1.0.0", "enabledFeatures": ["JSON-RPC 2.0", "WebSocket RPC"]}, None

        elif method == "aria2.getSessionInfo":
            return {"sessionId": "test-session"}, None

        elif method == "aria2.shutdown":
            return "OK", None

        elif method == "aria2.forceShutdown":
            return "OK", None

        elif method == "aria2.saveSession":
            return "OK", None

        else:
            return None, {"code": 1, "message": f"Method {method} not found"}

    async def start(self):
        """Start the mock server"""
        def find_free_port():
            with socket.socket() as s:
                s.bind(('', 0))
                return s.getsockname()[1]
        
        self.port = find_free_port()
        
        server = self
        class RequestHandler(BaseHTTPRequestHandler):
            def do_POST(self):
                content_length = int(self.headers.get('Content-Length', 0))
                post_data = self.rfile.read(content_length)
                
                try:
                    request = json.loads(post_data.decode('utf-8'))
                    method = request.get('method')
                    params = request.get('params', [])
                    
                    result, error = server._handle_method(method, params)
                    
                    if error:
                        response = {
                            "jsonrpc": "2.0",
                            "id": request.get('id'),
                            "error": error
                        }
                    else:
                        response = {
                            "jsonrpc": "2.0",
                            "id": request.get('id'),
                            "result": result
                        }
                    
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/json')
                    self.end_headers()
                    self.wfile.write(json.dumps(response).encode('utf-8'))
                except Exception as e:
                    self.send_response(500)
                    self.send_header('Content-Type', 'application/json')
                    self.end_headers()
                    self.wfile.write(json.dumps({"error": {"code": -32603, "message": str(e)}}).encode('utf-8'))
            
            def log_message(self, format, *args):
                pass  # Suppress logging
        
        self._server = HTTPServer(('localhost', self.port), RequestHandler)
        self._thread = Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        await asyncio.sleep(0.1)  # Wait for server to start

    async def stop(self):
        """Stop the mock server"""
        if self._server:
            self._server.shutdown()
            self._server.server_close()
            if self._thread:
                self._thread.join(timeout=1.0)


@pytest_asyncio.fixture
async def rpc_server():
    server = MockAria2Server()
    await server.start()
    yield server
    await server.stop()


@pytest_asyncio.fixture
async def rpc_server_with_token():
    server = MockAria2Server(token="test-token")
    await server.start()
    yield server
    await server.stop()


@pytest.fixture
def rpc_url(rpc_server: MockAria2Server) -> str:
    return f"http://localhost:{rpc_server.port}/jsonrpc"


@pytest.fixture
def rpc_url_with_token(rpc_server_with_token: MockAria2Server) -> str:
    return f"http://localhost:{rpc_server_with_token.port}/jsonrpc"
