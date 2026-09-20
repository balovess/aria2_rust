"use strict";
var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
  // If the importer is in node compatibility mode or this is not an ESM
  // file that has been converted to a CommonJS file using a Babel-
  // compatible transform (i.e. "__esModule" has not been set), then set
  // "default" to the CommonJS "module.exports" for node compatibility.
  isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
  mod
));
var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

// src/index.ts
var index_exports = {};
__export(index_exports, {
  Aria2Client: () => Aria2Client,
  Aria2Error: () => Aria2Error,
  Aria2EventEmitter: () => Aria2EventEmitter,
  AuthError: () => AuthError,
  ConnectionError: () => ConnectionError,
  DownloadStatus: () => DownloadStatus,
  EventType: () => EventType,
  PositionMode: () => PositionMode,
  RpcError: () => RpcError,
  TimeoutError: () => TimeoutError
});
module.exports = __toCommonJS(index_exports);

// src/transport.ts
var import_ws = __toESM(require("ws"));

// src/errors.ts
var Aria2Error = class extends Error {
  code;
  constructor(message, code = -1) {
    super(message);
    this.name = "Aria2Error";
    this.code = code;
  }
};
var ConnectionError = class extends Aria2Error {
  constructor(message) {
    super(message, -2);
    this.name = "ConnectionError";
  }
};
var AuthError = class extends Aria2Error {
  constructor(message) {
    super(message, -3);
    this.name = "AuthError";
  }
};
var RpcError = class extends Aria2Error {
  constructor(message, code) {
    super(message, code);
    this.name = "RpcError";
  }
};
var TimeoutError = class extends Aria2Error {
  constructor(message) {
    super(message, -4);
    this.name = "TimeoutError";
  }
};

// src/transport.ts
function isAuthRpcError(code, message) {
  if (code === -32001) return true;
  const normalized = message.toLowerCase();
  return [
    "unauthorized",
    "auth fail",
    "authentication",
    "authorization",
    "invalid token",
    "token required"
  ].some((marker) => normalized.includes(marker));
}
function buildParams(token, params) {
  const result = [];
  if (token) {
    result.push(`token:${token}`);
  }
  result.push(...params);
  return result;
}
var HttpTransport = class {
  url;
  token;
  timeout;
  nextId = 1;
  closed = false;
  pending = /* @__PURE__ */ new Map();
  constructor(url, options) {
    this.url = url;
    this.token = options?.token ?? options?.secret;
    this.timeout = options?.timeout ?? 3e4;
  }
  async sendRequest(method, params) {
    if (this.closed) {
      throw new ConnectionError("Transport closed");
    }
    const id = this.nextId++;
    const request = {
      jsonrpc: "2.0",
      id,
      method,
      params: buildParams(this.token, params)
    };
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);
    const pendingRequest = { controller, closed: false };
    this.pending.set(id, pendingRequest);
    try {
      const response = await fetch(this.url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(request),
        signal: controller.signal
      });
      let data;
      try {
        data = await response.json();
      } catch {
        if (!response.ok) {
          throw new ConnectionError(`HTTP ${response.status}: ${response.statusText}`);
        }
        throw new ConnectionError("Invalid JSON response");
      }
      if (data.error) {
        if (isAuthRpcError(data.error.code, data.error.message)) {
          throw new AuthError(data.error.message);
        }
        throw new RpcError(data.error.message, data.error.code);
      }
      if (!response.ok) {
        throw new ConnectionError(`HTTP ${response.status}: ${response.statusText}`);
      }
      return data.result;
    } catch (err) {
      if (err instanceof RpcError || err instanceof AuthError || err instanceof ConnectionError) {
        throw err;
      }
      if (pendingRequest.closed) {
        throw new ConnectionError("Transport closed");
      }
      if (err instanceof DOMException && err.name === "AbortError") {
        throw new TimeoutError(`Request timed out after ${this.timeout}ms`);
      }
      if (err instanceof Error && err.name === "AbortError") {
        throw new TimeoutError(`Request timed out after ${this.timeout}ms`);
      }
      throw new ConnectionError(err instanceof Error ? err.message : String(err));
    } finally {
      clearTimeout(timer);
      this.pending.delete(id);
    }
  }
  async close() {
    if (this.closed) return;
    this.closed = true;
    for (const pendingRequest of this.pending.values()) {
      pendingRequest.closed = true;
      pendingRequest.controller.abort();
    }
  }
};
var WebSocketTransport = class {
  url;
  token;
  timeout;
  nextId = 1;
  ws = null;
  pending = /* @__PURE__ */ new Map();
  onEvent = null;
  connectPromise = null;
  pendingWs = null;
  pendingConnectReject = null;
  closed = false;
  constructor(url, options, onEvent) {
    this.url = url;
    this.token = options?.token ?? options?.secret;
    this.timeout = options?.timeout ?? 3e4;
    this.onEvent = onEvent ?? null;
  }
  setEventHandler(handler) {
    this.onEvent = handler;
  }
  async ensureConnection() {
    if (this.closed) {
      throw new ConnectionError("Transport closed");
    }
    if (this.ws && this.ws.readyState === import_ws.default.OPEN) {
      return;
    }
    if (this.connectPromise) {
      await this.connectPromise;
      return;
    }
    this.connectPromise = new Promise((resolve, reject) => {
      const ws = new import_ws.default(this.url);
      let settled = false;
      const cleanup = () => {
        ws.removeListener("open", openHandler);
        ws.removeListener("error", errorHandler);
        ws.removeListener("close", closeHandler);
        if (this.pendingWs === ws) this.pendingWs = null;
        if (this.pendingConnectReject === rejectConnection) {
          this.pendingConnectReject = null;
        }
      };
      const rejectConnection = (error) => {
        if (settled) return;
        settled = true;
        cleanup();
        this.connectPromise = null;
        reject(error);
      };
      const openHandler = () => {
        if (settled) return;
        settled = true;
        cleanup();
        this.ws = ws;
        this.connectPromise = null;
        resolve();
      };
      const errorHandler = (err) => {
        rejectConnection(new ConnectionError(err.message));
      };
      const closeHandler = () => {
        rejectConnection(new ConnectionError("WebSocket connection closed"));
        this.rejectAllPending(new ConnectionError("WebSocket connection closed"));
      };
      this.pendingWs = ws;
      this.pendingConnectReject = rejectConnection;
      ws.once("open", openHandler);
      ws.once("error", errorHandler);
      ws.once("close", closeHandler);
      ws.on("message", (data) => {
        this.handleMessage(data);
      });
    });
    await this.connectPromise;
  }
  handleMessage(data) {
    let parsed;
    try {
      parsed = JSON.parse(String(data));
    } catch {
      return;
    }
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
      return;
    }
    const obj = parsed;
    if (typeof obj.method === "string" && !("id" in obj)) {
      const notification = obj;
      if (this.onEvent) {
        this.onEvent(notification.method, Array.isArray(notification.params) ? notification.params : []);
      }
      return;
    }
    if ("id" in obj) {
      const response = obj;
      const pending = this.pending.get(response.id);
      if (!pending) return;
      clearTimeout(pending.timer);
      this.pending.delete(response.id);
      if (response.error && typeof response.error === "object") {
        pending.reject(new RpcError(
          typeof response.error.message === "string" ? response.error.message : "Unknown RPC error",
          typeof response.error.code === "number" ? response.error.code : -1
        ));
      } else if ("error" in response) {
        pending.reject(new RpcError("Malformed RPC error response", -1));
      } else {
        pending.resolve(response.result);
      }
    }
  }
  rejectAllPending(error) {
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(error);
      this.pending.delete(id);
    }
  }
  async sendRequest(method, params) {
    await this.ensureConnection();
    const id = this.nextId++;
    const request = {
      jsonrpc: "2.0",
      id,
      method,
      params: buildParams(this.token, params)
    };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new TimeoutError(`Request timed out after ${this.timeout}ms`));
      }, this.timeout);
      this.pending.set(id, { resolve, reject, timer });
      const ws = this.ws;
      if (!ws || ws.readyState !== import_ws.default.OPEN) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(new ConnectionError("WebSocket is not connected"));
        return;
      }
      try {
        ws.send(JSON.stringify(request), (err) => {
          if (err) {
            clearTimeout(timer);
            this.pending.delete(id);
            reject(new ConnectionError(err.message));
          }
        });
      } catch (err) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(new ConnectionError(err instanceof Error ? err.message : String(err)));
      }
    });
  }
  async close() {
    if (this.closed) return;
    this.closed = true;
    const pendingReject = this.pendingConnectReject;
    this.pendingConnectReject = null;
    if (this.pendingWs) {
      const pendingWs = this.pendingWs;
      pendingWs.removeAllListeners();
      pendingWs.once("error", () => {
      });
      pendingWs.terminate();
      this.pendingWs = null;
    }
    this.connectPromise = null;
    pendingReject?.(new ConnectionError("Transport closed"));
    if (this.ws) {
      this.ws.removeAllListeners();
      this.ws.close();
      this.ws = null;
    }
    this.rejectAllPending(new ConnectionError("Transport closed"));
  }
};

// src/events.ts
var import_events = require("events");
var import_ws2 = __toESM(require("ws"));

// src/types.ts
var PositionMode = /* @__PURE__ */ ((PositionMode2) => {
  PositionMode2["SetFromStart"] = "POS_SET";
  PositionMode2["MoveFromStart"] = "POS_CUR";
  PositionMode2["SetFromEnd"] = "POS_END";
  return PositionMode2;
})(PositionMode || {});
var EventType = /* @__PURE__ */ ((EventType2) => {
  EventType2["DownloadStart"] = "aria2.onDownloadStart";
  EventType2["DownloadPause"] = "aria2.onDownloadPause";
  EventType2["DownloadStop"] = "aria2.onDownloadStop";
  EventType2["DownloadComplete"] = "aria2.onDownloadComplete";
  EventType2["DownloadError"] = "aria2.onDownloadError";
  EventType2["BtDownloadComplete"] = "aria2.onBtDownloadComplete";
  return EventType2;
})(EventType || {});
var DownloadStatus = /* @__PURE__ */ ((DownloadStatus2) => {
  DownloadStatus2["Active"] = "active";
  DownloadStatus2["Waiting"] = "waiting";
  DownloadStatus2["Paused"] = "paused";
  DownloadStatus2["Error"] = "error";
  DownloadStatus2["Complete"] = "complete";
  DownloadStatus2["Removed"] = "removed";
  return DownloadStatus2;
})(DownloadStatus || {});

// src/events.ts
var EVENT_MAP = {
  ["aria2.onDownloadStart" /* DownloadStart */]: "downloadStart",
  ["aria2.onDownloadPause" /* DownloadPause */]: "downloadPause",
  ["aria2.onDownloadStop" /* DownloadStop */]: "downloadStop",
  ["aria2.onDownloadComplete" /* DownloadComplete */]: "downloadComplete",
  ["aria2.onDownloadError" /* DownloadError */]: "downloadError",
  ["aria2.onBtDownloadComplete" /* BtDownloadComplete */]: "btDownloadComplete"
};
var TERMINAL_EVENT_NAMES = [
  "downloadStop",
  "downloadComplete",
  "downloadError",
  "btDownloadComplete"
];
var MAX_RECONNECT_RETRIES = 5;
var BASE_RECONNECT_DELAY = 1e3;
var Aria2EventEmitter = class extends import_events.EventEmitter {
  wsUrl;
  ws = null;
  pendingWs = null;
  reconnectAttempts = 0;
  reconnectTimer = null;
  closed = false;
  connectPromise = null;
  pendingConnectReject = null;
  terminalWaiters = /* @__PURE__ */ new Set();
  constructor(wsUrl, _options) {
    super();
    this.wsUrl = wsUrl;
  }
  async connect() {
    if (this.closed) {
      throw new ConnectionError("Emitter has been closed");
    }
    if (this.ws && this.ws.readyState === import_ws2.default.OPEN) {
      return;
    }
    if (this.connectPromise) {
      await this.connectPromise;
      return;
    }
    this.connectPromise = this.doConnect();
    try {
      await this.connectPromise;
    } finally {
      this.connectPromise = null;
    }
  }
  /**
   * Wait for a terminal event for one GID without polling status.
   *
   * Events for other GIDs and non-terminal transitions are ignored. Register
   * the wait before submitting a task when a fast completion must not be
   * missed. The timeout covers waiting for the event after the WebSocket is
   * connected; omit it for an unbounded download.
   */
  async waitForTerminal(gid, timeoutMs) {
    if (typeof gid !== "string" || gid.length === 0) {
      throw new TypeError("gid must be a non-empty string");
    }
    if (timeoutMs !== void 0 && (!Number.isFinite(timeoutMs) || timeoutMs <= 0)) {
      throw new TypeError("timeoutMs must be a positive number or undefined");
    }
    let timer = null;
    let settled = false;
    let cleanup = () => {
    };
    let startTimer = () => {
    };
    let rejectClosed = () => {
    };
    const result = new Promise((resolve, reject) => {
      const onEvent = (event) => {
        if (event.gid !== gid) return;
        settled = true;
        cleanup();
        resolve(event);
      };
      cleanup = () => {
        for (const eventName of TERMINAL_EVENT_NAMES) {
          this.off(eventName, onEvent);
        }
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
        this.terminalWaiters.delete(rejectClosed);
      };
      for (const eventName of TERMINAL_EVENT_NAMES) {
        this.on(eventName, onEvent);
      }
      startTimer = () => {
        if (timeoutMs === void 0 || settled) return;
        timer = setTimeout(() => {
          if (settled) return;
          settled = true;
          cleanup();
          reject(new TimeoutError(`Timed out waiting for terminal event for GID ${gid}`));
        }, timeoutMs);
      };
      rejectClosed = (error) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(error);
      };
      this.terminalWaiters.add(rejectClosed);
    });
    try {
      await this.connect();
      startTimer();
      return await result;
    } catch (error) {
      cleanup();
      throw error;
    }
  }
  async doConnect() {
    return new Promise((resolve, reject) => {
      const ws = new import_ws2.default(this.wsUrl);
      this.pendingWs = ws;
      let settled = false;
      const cleanup = () => {
        ws.removeListener("open", openHandler);
        ws.removeListener("error", errorHandler);
        ws.removeListener("close", closeHandler);
        if (this.pendingWs === ws) {
          this.pendingWs = null;
        }
        if (this.pendingConnectReject === rejectPendingConnection) {
          this.pendingConnectReject = null;
        }
      };
      const rejectConnection = (message) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(new ConnectionError(message));
      };
      const rejectPendingConnection = (error) => {
        rejectConnection(error.message);
      };
      const openHandler = () => {
        if (this.closed) {
          rejectConnection("Emitter has been closed");
          ws.close();
          return;
        }
        settled = true;
        cleanup();
        this.ws = ws;
        this.reconnectAttempts = 0;
        this.setupMessageHandler(ws);
        this.setupCloseHandler(ws);
        resolve();
      };
      const errorHandler = (err) => {
        rejectConnection(this.closed ? "Emitter has been closed" : err.message);
      };
      const closeHandler = (code) => {
        rejectConnection(
          this.closed ? "Emitter has been closed" : `WebSocket closed before connection established (code ${code})`
        );
      };
      this.pendingConnectReject = rejectPendingConnection;
      ws.once("open", openHandler);
      ws.once("error", errorHandler);
      ws.once("close", closeHandler);
    });
  }
  setupMessageHandler(ws) {
    ws.on("message", (data) => {
      let parsed;
      try {
        parsed = JSON.parse(String(data));
      } catch {
        return;
      }
      if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
        return;
      }
      const obj = parsed;
      if (typeof obj.method !== "string") return;
      const method = obj.method;
      const eventName = EVENT_MAP[method];
      if (!eventName) return;
      const params = Array.isArray(obj.params) ? obj.params : [];
      const details = params[0];
      const detailsObject = details !== null && typeof details === "object" ? details : void 0;
      const gid = typeof detailsObject?.gid === "string" ? detailsObject.gid : String(details);
      const event = {
        type: method,
        gid
      };
      const errorCode = detailsObject?.errorCode;
      if (typeof errorCode === "number" && Number.isSafeInteger(errorCode)) {
        event.errorCode = errorCode;
      } else if (typeof errorCode === "string" && /^-?\d+$/.test(errorCode)) {
        const parsed2 = Number(errorCode);
        if (Number.isSafeInteger(parsed2)) event.errorCode = parsed2;
      }
      if (Array.isArray(detailsObject?.files)) {
        event.files = detailsObject.files;
      }
      this.emit(eventName, event);
    });
  }
  setupCloseHandler(ws) {
    ws.once("close", (code, reason) => {
      if (this.ws === ws) {
        this.ws = null;
      }
      if (!this.closed) {
        this.emit("close", code, reason.toString());
        this.attemptReconnect();
      }
    });
    ws.once("error", () => {
      if (this.ws === ws) {
        this.ws = null;
      }
    });
  }
  attemptReconnect() {
    if (this.closed) return;
    if (this.reconnectAttempts >= MAX_RECONNECT_RETRIES) {
      this.emit("reconnecting", false, this.reconnectAttempts);
      return;
    }
    const delay = BASE_RECONNECT_DELAY * Math.pow(2, this.reconnectAttempts);
    this.reconnectAttempts++;
    this.emit("reconnecting", true, this.reconnectAttempts);
    this.reconnectTimer = setTimeout(async () => {
      if (this.closed) return;
      try {
        await this.connect();
      } catch {
        this.attemptReconnect();
      }
    }, delay);
  }
  async close() {
    this.closed = true;
    for (const reject of this.terminalWaiters) {
      reject(new ConnectionError("Emitter has been closed"));
    }
    this.terminalWaiters.clear();
    const pendingWs = this.pendingWs;
    const pendingReject = this.pendingConnectReject;
    this.pendingConnectReject = null;
    pendingReject?.(new ConnectionError("Emitter has been closed"));
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.removeAllListeners();
      this.ws.close();
      this.ws = null;
    }
    if (pendingWs) {
      pendingWs.removeAllListeners();
      pendingWs.once("error", () => {
      });
      pendingWs.terminate();
      if (this.pendingWs === pendingWs) {
        this.pendingWs = null;
      }
    }
    this.connectPromise = null;
    this.removeAllListeners();
  }
};

// src/client.ts
var DEFAULT_URL = "http://localhost:6800/jsonrpc";
function parseObjectResult(result, method) {
  if (result === null || typeof result !== "object" || Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  return result;
}
function parseObjectListResult(result, method) {
  if (!Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  for (const [index, item] of result.entries()) {
    if (item === null || typeof item !== "object" || Array.isArray(item)) {
      throw new Aria2Error(`Unexpected item type for ${method} at index ${index}`);
    }
  }
  return result;
}
function parseArrayResult(result, method) {
  if (!Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  return result;
}
function parseStringListResult(result, method, expectedLength) {
  if (!Array.isArray(result)) {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  if (expectedLength !== void 0 && result.length !== expectedLength) {
    throw new Aria2Error(
      `Unexpected result length for ${method}: expected ${expectedLength}, got ${result.length}`
    );
  }
  for (const [index, item] of result.entries()) {
    if (typeof item !== "string") {
      throw new Aria2Error(`Unexpected item type for ${method} at index ${index}`);
    }
  }
  return result;
}
function parseChangeUriCounts(result) {
  if (!Array.isArray(result)) {
    throw new Aria2Error("Unexpected result type for changeUri");
  }
  if (result.length !== 2) {
    throw new Aria2Error(
      `Unexpected result length for changeUri: expected 2, got ${result.length}`
    );
  }
  return result.map((item, index) => {
    if (typeof item === "string" && /^(0|[1-9]\d*)$/.test(item)) return item;
    if (typeof item === "number" && Number.isSafeInteger(item) && item >= 0) {
      return String(item);
    }
    throw new Aria2Error(`Unexpected item type for changeUri at index ${index}`);
  });
}
function parseStringResult(result, method) {
  if (typeof result !== "string") {
    throw new Aria2Error(`Unexpected result type for ${method}`);
  }
  return result;
}
function requireNonNegativeInteger(value, name) {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new TypeError(`${name} must be a non-negative safe integer`);
  }
}
function requireStringList(value, name, allowEmpty = true) {
  if (!Array.isArray(value) || !allowEmpty && value.length === 0) {
    throw new TypeError(`${name} must be a non-empty string array`);
  }
  if (value.some((item) => typeof item !== "string")) {
    throw new TypeError(`${name} must contain only strings`);
  }
}
function requirePositionMode(value) {
  if (value !== "POS_SET" && value !== "POS_CUR" && value !== "POS_END") {
    throw new TypeError("mode must be POS_SET, POS_CUR, or POS_END");
  }
}
function requireBuffer(value, name) {
  if (!Buffer.isBuffer(value)) {
    throw new TypeError(`${name} must be a Buffer`);
  }
}
function requireGid(value) {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError("gid must be a non-empty string");
  }
}
function requireRecord(value, name) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError(`${name} must be an object`);
  }
}
function httpToWs(url) {
  if (url.startsWith("https://")) {
    return url.replace("https://", "wss://");
  }
  if (url.startsWith("http://")) {
    return url.replace("http://", "ws://");
  }
  return url;
}
var Aria2Client = class {
  transport;
  eventEmitter = null;
  url;
  options;
  constructor(url, options) {
    this.url = url ?? DEFAULT_URL;
    this.options = options;
    if (this.url.startsWith("ws://") || this.url.startsWith("wss://")) {
      this.transport = new WebSocketTransport(this.url, options);
    } else {
      this.transport = new HttpTransport(this.url, options);
    }
  }
  async ensureEventEmitter() {
    const emitter = this.getOrCreateEventEmitter();
    await emitter.connect();
    return emitter;
  }
  getOrCreateEventEmitter() {
    if (this.eventEmitter) {
      return this.eventEmitter;
    }
    const wsUrl = httpToWs(this.url);
    this.eventEmitter = new Aria2EventEmitter(wsUrl, this.options);
    return this.eventEmitter;
  }
  /** Connect the notification WebSocket before starting a download. */
  async connectEvents() {
    return this.ensureEventEmitter();
  }
  async call(method, params = []) {
    return await this.transport.sendRequest(method, params);
  }
  async addUri(uris, options, position) {
    requireStringList(uris, "uris", false);
    if (options !== void 0) requireRecord(options, "options");
    if (position !== void 0) requireNonNegativeInteger(position, "position");
    const params = [uris];
    if (options !== void 0 || position !== void 0) params.push(options ?? {});
    if (position !== void 0) params.push(position);
    const result = await this.transport.sendRequest("aria2.addUri", params);
    return parseStringResult(result, "addUri");
  }
  async addTorrent(torrent, options, webSeedUris, position) {
    requireBuffer(torrent, "torrent");
    if (options !== void 0) requireRecord(options, "options");
    if (webSeedUris !== void 0) requireStringList(webSeedUris, "webSeedUris");
    if (position !== void 0) requireNonNegativeInteger(position, "position");
    const params = [torrent.toString("base64")];
    if (webSeedUris !== void 0 || options !== void 0 || position !== void 0) {
      params.push(webSeedUris ?? []);
    }
    if (options !== void 0 || position !== void 0) params.push(options ?? {});
    if (position !== void 0) params.push(position);
    const result = await this.transport.sendRequest("aria2.addTorrent", params);
    return parseStringResult(result, "addTorrent");
  }
  async addMetalink(metalink, options, position) {
    requireBuffer(metalink, "metalink");
    if (options !== void 0) requireRecord(options, "options");
    if (position !== void 0) requireNonNegativeInteger(position, "position");
    const params = [metalink.toString("base64")];
    if (options !== void 0) params.push(options);
    else if (position !== void 0) params.push({});
    if (position !== void 0) params.push(position);
    const result = await this.transport.sendRequest("aria2.addMetalink", params);
    return parseStringListResult(result, "addMetalink");
  }
  async remove(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.remove", [gid]);
    return parseStringResult(result, "remove");
  }
  async pause(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.pause", [gid]);
    return parseStringResult(result, "pause");
  }
  async unpause(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.unpause", [gid]);
    return parseStringResult(result, "unpause");
  }
  async forcePause(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.forcePause", [gid]);
    return parseStringResult(result, "forcePause");
  }
  async forceRemove(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.forceRemove", [gid]);
    return parseStringResult(result, "forceRemove");
  }
  async pauseAll() {
    const result = await this.transport.sendRequest("aria2.pauseAll", []);
    return parseStringResult(result, "pauseAll");
  }
  async forcePauseAll() {
    const result = await this.transport.sendRequest("aria2.forcePauseAll", []);
    return parseStringResult(result, "forcePauseAll");
  }
  async unpauseAll() {
    const result = await this.transport.sendRequest("aria2.unpauseAll", []);
    return parseStringResult(result, "unpauseAll");
  }
  async changePosition(gid, position, mode) {
    requireGid(gid);
    requireNonNegativeInteger(position, "position");
    requirePositionMode(mode);
    const result = await this.transport.sendRequest("aria2.changePosition", [gid, position, mode]);
    if (typeof result === "number" && Number.isSafeInteger(result) && result >= 0) {
      return result;
    }
    if (typeof result === "string" && /^(0|[1-9]\d*)$/.test(result)) {
      const parsed = Number(result);
      if (Number.isSafeInteger(parsed)) return parsed;
    }
    throw new Aria2Error(`Unexpected result type for changePosition: ${typeof result}`);
  }
  async changeUri(gid, fileIndex, deleteUris, addUris, position) {
    requireGid(gid);
    requireNonNegativeInteger(fileIndex, "fileIndex");
    requireStringList(deleteUris, "deleteUris");
    requireStringList(addUris, "addUris");
    if (position !== void 0) requireNonNegativeInteger(position, "position");
    const params = [gid, fileIndex, deleteUris, addUris];
    if (position !== void 0) params.push(position);
    const result = await this.transport.sendRequest("aria2.changeUri", params);
    return parseChangeUriCounts(result);
  }
  async tellStatus(gid, keys) {
    requireGid(gid);
    if (keys !== void 0) requireStringList(keys, "keys");
    const params = [gid];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest("aria2.tellStatus", params);
    return parseObjectResult(result, "tellStatus");
  }
  async getFiles(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.getFiles", [gid]);
    return parseObjectListResult(result, "getFiles");
  }
  async getUris(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.getUris", [gid]);
    return parseObjectListResult(result, "getUris");
  }
  async getServers(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.getServers", [gid]);
    return parseObjectListResult(result, "getServers");
  }
  async getPeers(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.getPeers", [gid]);
    return parseObjectListResult(result, "getPeers");
  }
  async getTrackers(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.getTrackers", [gid]);
    return parseObjectListResult(result, "getTrackers");
  }
  async getDhtStatus() {
    const result = await this.transport.sendRequest("aria2.getDhtStatus", []);
    return parseObjectResult(result, "getDhtStatus");
  }
  async tellActive(keys) {
    if (keys !== void 0) requireStringList(keys, "keys");
    const params = [];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest("aria2.tellActive", params);
    return parseObjectListResult(result, "tellActive");
  }
  async tellWaiting(offset, num, keys) {
    requireNonNegativeInteger(offset, "offset");
    requireNonNegativeInteger(num, "num");
    if (keys !== void 0) requireStringList(keys, "keys");
    const params = [offset, num];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest("aria2.tellWaiting", params);
    return parseObjectListResult(result, "tellWaiting");
  }
  async tellStopped(offset, num, keys) {
    requireNonNegativeInteger(offset, "offset");
    requireNonNegativeInteger(num, "num");
    if (keys !== void 0) requireStringList(keys, "keys");
    const params = [offset, num];
    if (keys) params.push(keys);
    const result = await this.transport.sendRequest("aria2.tellStopped", params);
    return parseObjectListResult(result, "tellStopped");
  }
  async getGlobalStat() {
    const result = await this.transport.sendRequest("aria2.getGlobalStat", []);
    return parseObjectResult(result, "getGlobalStat");
  }
  async purgeDownloadResult() {
    const result = await this.transport.sendRequest("aria2.purgeDownloadResult", []);
    return parseStringResult(result, "purgeDownloadResult");
  }
  async removeDownloadResult(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.removeDownloadResult", [gid]);
    return parseStringResult(result, "removeDownloadResult");
  }
  async getGlobalOption() {
    const result = await this.transport.sendRequest("aria2.getGlobalOption", []);
    return parseObjectResult(result, "getGlobalOption");
  }
  async changeGlobalOption(options) {
    requireRecord(options, "options");
    const result = await this.transport.sendRequest("aria2.changeGlobalOption", [options]);
    return parseStringResult(result, "changeGlobalOption");
  }
  async getOption(gid) {
    requireGid(gid);
    const result = await this.transport.sendRequest("aria2.getOption", [gid]);
    return parseObjectResult(result, "getOption");
  }
  async changeOption(gid, options) {
    requireGid(gid);
    requireRecord(options, "options");
    const result = await this.transport.sendRequest("aria2.changeOption", [gid, options]);
    return parseStringResult(result, "changeOption");
  }
  async getVersion() {
    const result = await this.transport.sendRequest("aria2.getVersion", []);
    return parseObjectResult(result, "getVersion");
  }
  async getSessionInfo() {
    const result = await this.transport.sendRequest("aria2.getSessionInfo", []);
    return parseObjectResult(result, "getSessionInfo");
  }
  async shutdown() {
    const result = await this.transport.sendRequest("aria2.shutdown", []);
    return parseStringResult(result, "shutdown");
  }
  async forceShutdown() {
    const result = await this.transport.sendRequest("aria2.forceShutdown", []);
    return parseStringResult(result, "forceShutdown");
  }
  async saveSession() {
    const result = await this.transport.sendRequest("aria2.saveSession", []);
    return parseStringResult(result, "saveSession");
  }
  async updateBrowserContext(context) {
    const result = await this.transport.sendRequest("aria2.updateBrowserContext", [context]);
    return parseStringResult(result, "updateBrowserContext");
  }
  async clearBrowserContext() {
    const result = await this.transport.sendRequest("aria2.clearBrowserContext", []);
    return parseStringResult(result, "clearBrowserContext");
  }
  async systemMulticall(calls) {
    if (!Array.isArray(calls)) throw new TypeError("calls must be an array");
    for (const [index, call] of calls.entries()) {
      if (call === null || typeof call !== "object" || typeof call.methodName !== "string" || call.methodName.length === 0 || call.params !== void 0 && !Array.isArray(call.params)) {
        throw new TypeError(`calls[${index}] must contain a methodName and optional params array`);
      }
    }
    const result = await this.transport.sendRequest("system.multicall", [calls]);
    return parseArrayResult(result, "system.multicall");
  }
  async systemListMethods() {
    const result = await this.transport.sendRequest("system.listMethods", []);
    return parseStringListResult(result, "system.listMethods");
  }
  async systemListNotifications() {
    const result = await this.transport.sendRequest("system.listNotifications", []);
    return parseStringListResult(result, "system.listNotifications");
  }
  registerEventListener(event, handler, once) {
    const emitter = this.getOrCreateEventEmitter();
    if (once) emitter.once(event, handler);
    else emitter.on(event, handler);
    void this.ensureEventEmitter().catch(() => {
    });
    return this;
  }
  on(event, handler) {
    return this.registerEventListener(event, handler, false);
  }
  once(event, handler) {
    return this.registerEventListener(event, handler, true);
  }
  off(event, handler) {
    this.eventEmitter?.off(event, handler);
    return this;
  }
  async close() {
    await this.transport.close();
    if (this.eventEmitter) {
      await this.eventEmitter.close();
      this.eventEmitter = null;
    }
  }
  destroy() {
    this.transport.close().catch(() => {
    });
    if (this.eventEmitter) {
      this.eventEmitter.close().catch(() => {
      });
      this.eventEmitter = null;
    }
  }
};
// Annotate the CommonJS export names for ESM import in node:
0 && (module.exports = {
  Aria2Client,
  Aria2Error,
  Aria2EventEmitter,
  AuthError,
  ConnectionError,
  DownloadStatus,
  EventType,
  PositionMode,
  RpcError,
  TimeoutError
});
