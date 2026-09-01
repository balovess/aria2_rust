# Browser Session Bridge Developer Guide

This guide is for developers building cloud-drive, hotlink, or browser-session download integrations on top of `aria2-core`.

## Scope

`aria2-core` provides a thread-safe browser session context for updating Cookies, User-Agent, and signed headers at runtime. HTTP HEAD, Range, redirect, and authentication retry requests read the latest snapshot immediately before sending, so token rotation does not require recreating a download task.

The project does not include a Chrome/CDP client or browser extension. Developers can obtain browser data by any authorized method and publish snapshots through the existing RPC interface.

## Global updates

```rust,no_run
use aria2_core::http::update_global_json;

fn on_browser_event(json: &str) -> Result<(), serde_json::Error> {
    update_global_json(json)
}
```

The JSON format is:

```json
{
  "cookie": "sid=abc; csrf=token",
  "user_agent": "Mozilla/5.0 ...",
  "headers": [
    ["X-Signature", "signed-value"],
    ["Authorization", "Bearer temporary-token"]
  ]
}
```

Each update replaces the complete snapshot. The bridge should send all currently valid values, not only the field that changed. Clear the session with `aria2_core::http::global_browser_context().clear()`.

## Isolated contexts

For multiple accounts or sites, create an isolated context instead of sharing the global context:

```rust,no_run
use aria2_core::http::{BrowserContext, BrowserContextUpdate, HttpRequestPolicy};

let context = BrowserContext::new();
context.replace(
    BrowserContextUpdate::new()
        .with_cookie("sid=abc")
        .with_user_agent("Mozilla/5.0 ...")
        .with_header("X-Signature", "signed-value"),
);
let policy = HttpRequestPolicy::default().with_browser_context(context.clone());
```

Explicit task headers have higher precedence and are not overwritten by the browser snapshot.

## Recommended architecture

```text
Browser extension / CDP client
          |
          v
Developer-owned bridge (WebSocket, named pipe, local HTTP, ...)
          |
          v
BrowserContext::replace_json / update_global_json
          |
          v
aria2-core HTTP request policy
```

The bridge owns browser permissions, origin validation, token expiry checks, and inter-process authentication. `aria2-core` stores the snapshot and applies it to HTTP requests.

## Update through aria2c RPC

After starting RPC on a standalone `aria2c` process, send this through JSON-RPC, XML-RPC, or WebSocket:

```powershell
$body = '{"jsonrpc":"2.0","id":1,"method":"aria2.updateBrowserContext","params":["token:replace-with-rpc-secret",{"cookie":"sid=abc","user_agent":"Mozilla/5.0","headers":[["X-Signature","signed-value"]]}]}'
Invoke-RestMethod -Uri http://127.0.0.1:6800/jsonrpc -Method Post -ContentType 'application/json' -Body $body
```

Use `aria2.clearBrowserContext` to clear the current session. Both methods require RPC authentication and credentials are never returned by `getGlobalOption`.

WebSocket clients use the existing `/jsonrpc` endpoint and send standard JSON-RPC messages:

```javascript
const socket = new WebSocket("ws://127.0.0.1:6800/jsonrpc");
socket.onopen = () => socket.send(JSON.stringify({
  jsonrpc: "2.0",
  id: 1,
  method: "aria2.updateBrowserContext",
  params: ["token:replace-with-rpc-secret", {
    cookie: "sid=abc",
    user_agent: "Mozilla/5.0",
    headers: [["X-Signature", "signed-value"]]
  }]
}));
```

A browser extension or CDP adapter can be implemented as an RPC client; the existing RPC service already covers HTTP JSON-RPC, XML-RPC, and WebSocket transports. Production integrations should add origin validation, token rotation, and domain isolation.

## Security requirements

- Bind to localhost only, or require a strong random authentication token.
- Never log Cookie, Authorization, or signature values.
- Call `clear()` when the browser exits, the account changes, or access is revoked.
- Publish complete snapshots so stale tokens cannot be combined with new cookies.
- The global context affects all HTTP downloads; use isolated contexts for multiple accounts.

## Not currently included

- Chrome DevTools Protocol WebSocket client
- Browser extension source
- A dedicated CDP protocol beyond the Cookie/Token update RPC methods
- Automatic per-domain isolation for the global context

External developers can build these transport pieces on top of this API. An in-tree adapter should also include local authorization, domain isolation, and credential-redaction tests.
