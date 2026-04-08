from __future__ import annotations

import asyncio
import json
from typing import Any, Callable, Dict, List, Optional, Protocol, runtime_checkable

import httpx

from .errors import AuthError, ConnectionError, RpcError, TimeoutError


@runtime_checkable
class Transport(Protocol):
    async def send_request(self, method: str, params: list) -> Any:
        ...

    async def close(self) -> None:
        ...


class HttpTransport:
    def __init__(
        self,
        url: str,
        token: Optional[str] = None,
        timeout: float = 30.0,
    ) -> None:
        self._url = url
        self._token = token
        self._timeout = timeout
        self._id_counter = 0
        self._client = httpx.AsyncClient(timeout=timeout)

    def _next_id(self) -> int:
        self._id_counter += 1
        return self._id_counter

    def _build_request(self, method: str, params: list) -> Dict[str, Any]:
        request_params: list = []
        if self._token is not None:
            request_params.append(f"token:{self._token}")
        request_params.extend(params)
        return {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": method,
            "params": request_params,
        }

    async def send_request(self, method: str, params: list) -> Any:
        payload = self._build_request(method, params)
        try:
            response = await self._client.post(self._url, json=payload)
            response.raise_for_status()
        except httpx.TimeoutException as exc:
            raise TimeoutError(f"Request timed out: {exc}") from exc
        except httpx.HTTPStatusError as exc:
            if exc.response.status_code in (401, 403):
                raise AuthError(f"Authentication failed: {exc}") from exc
            raise ConnectionError(f"HTTP error {exc.response.status_code}: {exc}") from exc
        except httpx.HTTPError as exc:
            raise ConnectionError(f"Connection error: {exc}") from exc

        data = response.json()
        if "error" in data:
            err = data["error"]
            err_msg = err.get("message", "Unknown RPC error")
            err_code = err.get("code", -1)
            if err_code in (1, 2):
                raise AuthError(err_msg, err_code)
            raise RpcError(err_msg, err_code)

        return data.get("result")

    async def close(self) -> None:
        await self._client.aclose()


class WebSocketTransport:
    def __init__(
        self,
        url: str,
        token: Optional[str] = None,
        timeout: float = 30.0,
    ) -> None:
        self._url = url
        self._token = token
        self._timeout = timeout
        self._id_counter = 0
        self._ws: Any = None
        self._pending: Dict[int, asyncio.Future] = {}
        self._listener_task: Optional[asyncio.Task] = None
        self._event_callback: Optional[Callable[[str, Dict[str, Any]], None]] = None
        self._connected = False

    def _next_id(self) -> int:
        self._id_counter += 1
        return self._id_counter

    def set_event_callback(
        self, callback: Callable[[str, Dict[str, Any]], None]
    ) -> None:
        self._event_callback = callback

    async def _ensure_connected(self) -> None:
        if self._connected and self._ws is not None:
            return
        try:
            import websockets

            self._ws = await websockets.connect(
                self._url, open_timeout=self._timeout
            )
            self._connected = True
            self._listener_task = asyncio.create_task(self._listen())
        except asyncio.TimeoutError as exc:
            raise TimeoutError(f"WebSocket connection timed out: {exc}") from exc
        except Exception as exc:
            raise ConnectionError(f"WebSocket connection failed: {exc}") from exc

    async def _listen(self) -> None:
        try:
            async for raw_message in self._ws:
                try:
                    message = json.loads(raw_message)
                except (json.JSONDecodeError, TypeError):
                    continue

                if "method" in message and message["method"].startswith("aria2.on"):
                    if self._event_callback is not None:
                        params = message.get("params", [{}])
                        event_params = params[0] if isinstance(params, list) and params else {}
                        if not isinstance(event_params, dict):
                            event_params = {}
                        try:
                            self._event_callback(message["method"], event_params)
                        except Exception:
                            pass
                    continue

                msg_id = message.get("id")
                if msg_id is not None and msg_id in self._pending:
                    future = self._pending.pop(msg_id)
                    if not future.done():
                        if "error" in message:
                            future.set_exception(
                                RpcError(
                                    message["error"].get("message", "Unknown RPC error"),
                                    message["error"].get("code", -1),
                                )
                            )
                        else:
                            future.set_result(message.get("result"))
        except asyncio.CancelledError:
            pass
        except Exception:
            for future in self._pending.values():
                if not future.done():
                    future.set_exception(
                        ConnectionError("WebSocket connection lost")
                    )
            self._pending.clear()
        finally:
            self._connected = False

    def _build_request(self, method: str, params: list) -> Dict[str, Any]:
        request_params: list = []
        if self._token is not None:
            request_params.append(f"token:{self._token}")
        request_params.extend(params)
        return {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": method,
            "params": request_params,
        }

    async def send_request(self, method: str, params: list) -> Any:
        await self._ensure_connected()
        payload = self._build_request(method, params)
        request_id = payload["id"]
        loop = asyncio.get_running_loop()
        future: asyncio.Future = loop.create_future()
        self._pending[request_id] = future

        try:
            await self._ws.send(json.dumps(payload))
        except Exception as exc:
            self._pending.pop(request_id, None)
            raise ConnectionError(f"Failed to send WebSocket message: {exc}") from exc

        try:
            return await asyncio.wait_for(future, timeout=self._timeout)
        except asyncio.TimeoutError:
            self._pending.pop(request_id, None)
            raise TimeoutError(f"Request timed out after {self._timeout}s")

    async def close(self) -> None:
        if self._listener_task is not None:
            self._listener_task.cancel()
            try:
                await self._listener_task
            except asyncio.CancelledError:
                pass
            self._listener_task = None

        if self._ws is not None:
            try:
                await self._ws.close()
            except Exception:
                pass
            self._ws = None

        for future in self._pending.values():
            if not future.done():
                future.set_exception(ConnectionError("Transport closed"))
        self._pending.clear()
        self._connected = False
