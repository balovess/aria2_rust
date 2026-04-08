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
  constructor(url, options) {
    this.url = url;
    this.token = options?.token ?? options?.secret;
    this.timeout = options?.timeout ?? 3e4;
  }
  async sendRequest(method, params) {
    const id = this.nextId++;
    const request = {
      jsonrpc: "2.0",
      id,
      method,
      params: buildParams(this.token, params)
    };
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeout);
    try {
      const response = await fetch(this.url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(request),
        signal: controller.signal
      });
      if (!response.ok) {
        throw new ConnectionError(`HTTP ${response.status}: ${response.statusText}`);
      }
      const data = await response.json();
      if (data.error) {
        if (data.error.code === 1) {
          throw new RpcError(data.error.message, data.error.code);
        }
        throw new RpcError(data.error.message, data.error.code);
      }
      return data.result;
    } catch (err) {
      if (err instanceof RpcError || err instanceof ConnectionError) {
        throw err;
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
    }
  }
  async close() {
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
    if (this.ws && this.ws.readyState === import_ws.default.OPEN) {
      return;
    }
    if (this.connectPromise) {
      await this.connectPromise;
      return;
    }
    this.connectPromise = new Promise((resolve, reject) => {
      const ws = new import_ws.default(this.url);
      ws.once("open", () => {
        this.ws = ws;
        this.connectPromise = null;
        resolve();
      });
      ws.once("error", (err) => {
        this.ws = null;
        this.connectPromise = null;
        reject(new ConnectionError(err.message));
      });
      ws.once("close", () => {
        this.ws = null;
        this.connectPromise = null;
        this.rejectAllPending(new ConnectionError("WebSocket connection closed"));
      });
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
    const obj = parsed;
    if ("method" in obj && !("id" in obj)) {
      const notification = obj;
      if (this.onEvent) {
        this.onEvent(notification.method, notification.params);
      }
      return;
    }
    if ("id" in obj) {
      const response = obj;
      const pending = this.pending.get(response.id);
      if (!pending) return;
      clearTimeout(pending.timer);
      this.pending.delete(response.id);
      if (response.error) {
        pending.reject(new RpcError(response.error.message, response.error.code));
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
      this.ws.send(JSON.stringify(request), (err) => {
        if (err) {
          clearTimeout(timer);
          this.pending.delete(id);
          reject(new ConnectionError(err.message));
        }
      });
    });
  }
  async close() {
    if (this.ws) {
      this.ws.removeAllListeners();
      this.ws.close();
      this.ws = null;
    }
    this.rejectAllPending(new ConnectionError("Transport closed"));
    this.connectPromise = null;
  }
};

// src/events.ts
var import_events = require("events");
var import_ws2 = __toESM(require("ws"));

// src/types.ts
var EventType = /* @__PURE__ */ ((EventType2) => {
  EventType2["DownloadStart"] = "aria2.onDownloadStart";
  EventType2["DownloadPause"] = "aria2.onDownloadPause";
  EventType2["DownloadStop"] = "aria2.onDownloadStop";
  EventType2["DownloadComplete"] = "aria2.onDownloadComplete";
  EventType2["DownloadError"] = "aria2.onDownloadError";
  EventType2["BtDownloadComplete"] = "aria2.onBtDownloadComplete";
  EventType2["BtDownloadError"] = "aria2.onBtDownloadError";
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
  ["aria2.onBtDownloadComplete" /* BtDownloadComplete */]: "btDownloadComplete",
  ["aria2.onBtDownloadError" /* BtDownloadError */]: "btDownloadError"
};
var MAX_RECONNECT_RETRIES = 5;
var BASE_RECONNECT_DELAY = 1e3;
var Aria2EventEmitter = class extends import_events.EventEmitter {
  wsUrl;
  ws = null;
  reconnectAttempts = 0;
  reconnectTimer = null;
  closed = false;
  connectPromise = null;
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
  async doConnect() {
    return new Promise((resolve, reject) => {
      const ws = new import_ws2.default(this.wsUrl);
      const openHandler = () => {
        ws.removeListener("error", errorHandler);
        this.ws = ws;
        this.reconnectAttempts = 0;
        this.setupMessageHandler(ws);
        this.setupCloseHandler(ws);
        resolve();
      };
      const errorHandler = (err) => {
        ws.removeListener("open", openHandler);
        reject(new ConnectionError(err.message));
      };
      ws.once("open", openHandler);
      ws.once("error", errorHandler);
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
      const obj = parsed;
      if (!("method" in obj)) return;
      const method = obj.method;
      const eventName = EVENT_MAP[method];
      if (!eventName) return;
      const params = obj.params ?? [];
      const gid = params[0]?.gid ?? String(params[0]);
      const event = {
        type: method,
        gid
      };
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
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.removeAllListeners();
      this.ws.close();
      this.ws = null;
    }
    this.connectPromise = null;
    this.removeAllListeners();
  }
};

// src/client.ts
var DEFAULT_URL = "http://localhost:6800/jsonrpc";
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
    if (this.eventEmitter) {
      return this.eventEmitter;
    }
    const wsUrl = httpToWs(this.url);
    this.eventEmitter = new Aria2EventEmitter(wsUrl, this.options);
    await this.eventEmitter.connect();
    return this.eventEmitter;
  }
  async addUri(uris, options) {
    const params = [uris];
    if (options) params.push(options);
    return await this.transport.sendRequest("aria2.addUri", params);
  }
  async addTorrent(torrent, options) {
    const params = [torrent.toString("base64")];
    if (options) params.push(options);
    return await this.transport.sendRequest("aria2.addTorrent", params);
  }
  async addMetalink(metalink, options) {
    const params = [metalink.toString("base64")];
    if (options) params.push(options);
    return await this.transport.sendRequest("aria2.addMetalink", params);
  }
  async remove(gid) {
    return await this.transport.sendRequest("aria2.remove", [gid]);
  }
  async pause(gid) {
    return await this.transport.sendRequest("aria2.pause", [gid]);
  }
  async unpause(gid) {
    return await this.transport.sendRequest("aria2.unpause", [gid]);
  }
  async forcePause(gid) {
    return await this.transport.sendRequest("aria2.forcePause", [gid]);
  }
  async forceRemove(gid) {
    return await this.transport.sendRequest("aria2.forceRemove", [gid]);
  }
  async forceUnpause(gid) {
    return await this.transport.sendRequest("aria2.forceUnpause", [gid]);
  }
  async tellStatus(gid, keys) {
    const params = [gid];
    if (keys) params.push(keys);
    return await this.transport.sendRequest("aria2.tellStatus", params);
  }
  async tellActive(keys) {
    const params = [];
    if (keys) params.push(keys);
    return await this.transport.sendRequest("aria2.tellActive", params);
  }
  async tellWaiting(offset, num, keys) {
    const params = [offset, num];
    if (keys) params.push(keys);
    return await this.transport.sendRequest("aria2.tellWaiting", params);
  }
  async tellStopped(offset, num, keys) {
    const params = [offset, num];
    if (keys) params.push(keys);
    return await this.transport.sendRequest("aria2.tellStopped", params);
  }
  async getGlobalStat() {
    return await this.transport.sendRequest("aria2.getGlobalStat", []);
  }
  async purgeDownloadResult() {
    return await this.transport.sendRequest("aria2.purgeDownloadResult", []);
  }
  async removeDownloadResult(gid) {
    return await this.transport.sendRequest("aria2.removeDownloadResult", [gid]);
  }
  async getGlobalOption() {
    return await this.transport.sendRequest("aria2.getGlobalOption", []);
  }
  async changeGlobalOption(options) {
    return await this.transport.sendRequest("aria2.changeGlobalOption", [options]);
  }
  async getOption(gid) {
    return await this.transport.sendRequest("aria2.getOption", [gid]);
  }
  async changeOption(gid, options) {
    return await this.transport.sendRequest("aria2.changeOption", [gid, options]);
  }
  async getVersion() {
    return await this.transport.sendRequest("aria2.getVersion", []);
  }
  async getSessionInfo() {
    return await this.transport.sendRequest("aria2.getSessionInfo", []);
  }
  async shutdown() {
    return await this.transport.sendRequest("aria2.shutdown", []);
  }
  async forceShutdown() {
    return await this.transport.sendRequest("aria2.forceShutdown", []);
  }
  async saveSession() {
    return await this.transport.sendRequest("aria2.saveSession", []);
  }
  on(event, handler) {
    this.ensureEventEmitter().then((emitter) => {
      emitter.on(event, handler);
    }).catch(() => {
      throw new ConnectionError(`Failed to connect event emitter for event: ${event}`);
    });
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
  RpcError,
  TimeoutError
});
