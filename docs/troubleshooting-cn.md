# aria2-rust 常见问题

## 程序无法启动

先确认使用的是正确的二进制和工作目录：

```text
aria2c --version
aria2c --help
```

源码构建使用 `cargo build -p aria2 --release`。Windows 程序名是 `aria2c.exe`。端口、日志、session 和下载目录使用相对路径时，都相对于当前工作目录。

## 配置报错或没有生效

```text
aria2c --conf-path=aria2.conf --check-config
```

配置文件每行一个 `name=value`，不要写 `--`。布尔值使用 `true`/`false`。优先级是内置默认值 < 环境变量 < 配置文件 < 命令行，因此命令行会覆盖配置文件。未知或当前未实现的兼容项会警告；以当前版本 `aria2c --help` 为准。

修复单个无效项：

```text
aria2c --conf-path=aria2.conf --repair-config
```

## RPC 连接失败

确认配置包含 `enable-rpc=true`，并检查地址和端口：

```text
aria2c --conf-path=aria2.conf
```

默认端点为 `http://127.0.0.1:6800/jsonrpc`。启用 `rpc-secret` 后，将 `token:<secret>` 作为 `params` 第一个元素；认证失败不是下载任务错误。远程访问还需要监听地址和防火墙允许连接。

## HTTPS 或 CORS 失败

`rpc-secure=true` 必须同时提供 PEM 格式的 `rpc-certificate` 和 `rpc-private-key`。浏览器调用 RPC 时，将 `rpc-cors-domain` 设置为实际页面来源；只有受控环境才使用 `rpc-allow-origin-all=true`。

## 下载速度为零或任务失败

先用 `aria2.tellStatus` 或控制台输出查看 `status`、`errorCode`、`errorMessage`。常见原因是 URL 不可达、TLS 证书校验失败、代理配置错误、超时过短或远端拒绝 Range 请求。不要先盲目增大 `split`；先确认单连接下载可用，再调整 `split` 和 `max-connection-per-server`。

## BitTorrent、Metalink 或 SFTP 不可用

用 `aria2.getVersion` 查看 `enabledFeatures`。默认构建包含 BitTorrent，不默认包含 Metalink 和 SFTP；重新构建时使用 `--features "metalink,sftp"`。仅在 feature 已启用且方法出现在 `system.listMethods` 中时使用对应 RPC 方法。

## 断点续传或 session 恢复异常

`.aria2` 控制文件用于单个下载的分片恢复；session 文件用于恢复任务列表，二者不是同一种文件。停止程序后再检查或移动这些文件，不要在运行期间编辑 session 文件。使用 `save-session` 保存未完成任务，使用 `input-file` 在下次启动时读取。
