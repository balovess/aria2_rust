from __future__ import annotations

import asyncio
import os
import shutil
import subprocess
import tempfile
from typing import Optional

import pytest


def find_aria2_rust_binary() -> Optional[str]:
    for name in ["aria2c-rust", "aria2_rust", "aria2c"]:
        path = shutil.which(name)
        if path is not None:
            return path
    project_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "..", ".."))
    for root, dirs, files in os.walk(project_root):
        for f in files:
            if f in ("aria2c-rust", "aria2_rust", "aria2c") or f.endswith(".exe"):
                base = os.path.splitext(f)[0]
                if base in ("aria2c-rust", "aria2_rust", "aria2c"):
                    return os.path.join(root, f)
    return None


class FileServer:
    def __init__(self, directory: str):
        self.directory = directory
        self._server: Optional[asyncio.Server] = None
        self.port: int = 0

    async def _handle_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        try:
            while True:
                data = await reader.read(65536)
                if not data:
                    break
                request = data.decode("utf-8", errors="replace")
                lines = request.split("\r\n")
                if not lines:
                    break
                parts = lines[0].split(" ")
                if len(parts) < 2:
                    break
                method, path = parts[0], parts[1]
                if path == "/":
                    path = "/index.html"
                file_path = os.path.join(self.directory, path.lstrip("/"))
                if os.path.isfile(file_path):
                    with open(file_path, "rb") as f:
                        content = f.read()
                    header = (
                        "HTTP/1.1 200 OK\r\n"
                        "Content-Type: application/octet-stream\r\n"
                        f"Content-Length: {len(content)}\r\n"
                        "Connection: close\r\n\r\n"
                    )
                    writer.write(header.encode("utf-8") + content)
                else:
                    header = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n"
                    writer.write(header.encode("utf-8"))
                await writer.drain()
                break
        except (ConnectionResetError, asyncio.IncompleteReadError):
            pass
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

    async def start(self):
        self._server = await asyncio.start_server(self._handle_client, "127.0.0.1", 0)
        self.port = self._server.sockets[0].getsockname()[1]

    async def stop(self):
        if self._server:
            self._server.close()
            await self._server.wait_closed()


class Aria2Server:
    def __init__(self, binary: str, port: int = 0, token: Optional[str] = None):
        self.binary = binary
        self.port = port
        self.token = token
        self._process: Optional[subprocess.Popen] = None
        self._dir = tempfile.mkdtemp(prefix="aria2_e2e_")

    async def start(self):
        cmd = [
            self.binary,
            "--enable-rpc",
            f"--rpc-listen-port=0",
            "--dir", self._dir,
        ]
        if self.token:
            cmd.extend(["--rpc-secret", self.token])
        self._process = subprocess.Popen(
            cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        await asyncio.sleep(1)
        if self._process.poll() is not None:
            raise RuntimeError(f"aria2-rust failed to start: {self._process.stderr.read().decode()}")

    async def stop(self):
        if self._process and self._process.poll() is None:
            self._process.terminate()
            try:
                self._process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._process.kill()
        shutil.rmtree(self._dir, ignore_errors=True)

    @property
    def rpc_url(self) -> str:
        return f"http://127.0.0.1:{self.port}/jsonrpc"


@pytest.fixture
async def test_file_server():
    tmpdir = tempfile.mkdtemp(prefix="aria2_test_files_")
    small_file = os.path.join(tmpdir, "small.txt")
    with open(small_file, "w") as f:
        f.write("Hello, aria2-rust! " * 100)
    medium_file = os.path.join(tmpdir, "medium.bin")
    with open(medium_file, "wb") as f:
        f.write(os.urandom(1024 * 100))
    server = FileServer(tmpdir)
    await server.start()
    yield server, tmpdir
    await server.stop()
    shutil.rmtree(tmpdir, ignore_errors=True)


@pytest.fixture
async def aria2_server():
    binary = find_aria2_rust_binary()
    if binary is None:
        pytest.skip("aria2-rust binary not found, skipping E2E tests")
    server = Aria2Server(binary, token="e2e-test-token")
    try:
        await server.start()
    except RuntimeError:
        pytest.skip("aria2-rust binary failed to start, skipping E2E tests")
    yield server
    await server.stop()
