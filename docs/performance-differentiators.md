# Performance Differentiators vs aria2_original

Last reviewed: 2026-08-28

本文档记录 `aria2-rust` 相对 `aria2_original` 的内部性能差异化实现。
它是实现和证据索引，不是完整兼容性结论，也不把 Rust-only 微基准解释为
相对原版 C++ aria2 的吞吐提升。

## 状态定义

| 状态 | 含义 |
| --- | --- |
| Implemented | 代码路径已经接入，且有 Rust-owned 单元测试、集成测试或生命周期测试。 |
| Implemented with boundary | 主要路径已经接入，但默认路径、feature gate、平台运行时或调度接线仍有限制。 |
| Measurement pending | 实现和局部正确性已有证据，但还没有可复现的跨实现端到端性能基线。 |

## 总览

| 领域 | `aria2_original` 基线 | `aria2-rust` 差异化实现 | 状态 |
| --- | --- | --- | --- |
| 磁盘 I/O | 同步文件写入和简单的后台写入队列，热路径可能在写入队列、锁和条件变量上串行化。 | Positioned I/O、写回缓存、阈值批处理、跨文件 coalescing；阻塞 syscall 移到 Tokio blocking pool，Linux 另有 opt-in `io_uring` backend。 | Implemented with boundary; Windows Rust-only comparison recorded, end-to-end measurement pending |
| 数据路径和内存 | Socket、协议解码、segment/piece 组装和落盘之间存在多次拥有式缓冲转换。 | 以 `bytes::Bytes` 传递不可变拥有缓冲，缓存和跨文件切片使用 O(1) slice；写入接口可直接消费 `Bytes`。 | Implemented with boundary |
| Hash 校验 | 大块 Piece 或完整文件校验可能占用当前执行流。 | 有界 hash worker、`spawn_blocking`、分块完整性 dispatcher、协作式让出和可取消生命周期。 | Implemented |
| BT peer / HAVE | Peer 管理、筛选和 HAVE 广播容易在高 peer 数量下重复遍历和重复编码。 | HashSet/VecDeque peer 生命周期、增量 piece 频率、一次序列化后并发广播 HAVE；endgame/piece pipeline 有界。 | Implemented; Measurement pending |
| DHT | K-Bucket 查询和 UDP 消息处理的并发度控制较弱。 | 二叉 bucket tree、固定大小 top-K heap、1024 入站队列、4 个入站 worker 和分 lane 的有界任务执行器。 | Implemented with boundary |
| 文件预分配 | 平台 syscall、权限和不支持的文件系统可能退化为长时间同步 zero-fill。 | Linux raw `fallocate` + `EOPNOTSUPP` 检测，Windows `SetFileValidData` 权限尝试，macOS `F_PREALLOCATE`，所有 fallback 都不占用 reactor。 | Implemented with boundary; cross-platform runtime evidence pending |
| RPC 控制面 | JSON-RPC 解析和命令执行容易在单一事件循环中排队。 | wire visitor、owned dispatch、只读 batch 最多 64 路并发、mutation barrier、重解析/转换工作移到 blocking pool。 | Implemented with boundary |
| 事件驱动热路径 | 部分空闲、接收和调度路径依赖重复扫描或无输入等待。 | uTP readiness、持久分片缓冲、有界 BT block pipeline、真实事件驱动 engine idle wait 和 deadline coordinator。 | Implemented; Measurement pending |

## 1. 磁盘 I/O：positioned write、写回缓存和批处理

### 原版差异

原版的 DirectDiskAdaptor / BufferedFileAdaptor 仍以同步 `write`、`pwrite` 或
等价的文件操作为基础。AsyncMemoryWriter 可以把部分工作移到后台，但网络线程到
写线程之间仍然有单一或少量队列、锁和条件变量的协调边界。

### Rust 实现

- [`PositionedDiskWriter`](../aria2-core/src/filesystem/positioned_disk_writer/) 使用
  Unix `pwrite` 或 Windows `seek_write`，按 offset 写入，不修改共享文件 cursor。
  文件打开、读写、truncate、flush 等可能阻塞的 syscall 放到
  `tokio::task::spawn_blocking`。
- 单文件 BT 下载默认使用
  [`CachedDiskWriter`](../aria2-core/src/filesystem/disk_writer/buffered.rs) 和
  [`WrDiskCache`](../aria2-core/src/filesystem/disk_cache/)。缓存用按 offset 的
  `BTreeMap` 保存不重叠 dirty ranges，默认有内存上限，并在 flush 时释放 map 锁，
  不把异步 I/O 放在 map 锁内。
- [`BatchedDiskWriter`](../aria2-core/src/filesystem/batched_disk_writer.rs) 按
  总字节数或 pending write 数量触发 flush；BT 多文件写入路径会先按文件和 offset
  排序，并合并相邻范围，减少 open、seek 和 write 次数。
- Linux 的 [`IoUringDiskWriter`](../aria2-core/src/filesystem/positioned_disk_writer/io_uring.rs)
  由 `io_uring` feature 显式启用。它是可选 backend，当前默认下载 pipeline 仍使用
  `PositionedDiskWriter`，因此不能把默认构建描述为全链路 io_uring。

### 兼容性边界

文件 offset、resume、flush 和 control-file 发布顺序保持原有语义；BT checkpoint
在发布 metadata 前先 flush payload。变化只在内部调度和写入实现，外部 RPC、CLI 和
下载 payload 的 offset 布局不变；control-file 的完整兼容性仍以兼容性矩阵为准。
`spawn_blocking` 解决的是 reactor 被同步 syscall 阻塞的问题，
并不意味着底层磁盘 syscall 本身不会等待。

### 证据入口

- [`positioned_disk_writer/tests.rs`](../aria2-core/src/filesystem/positioned_disk_writer/tests.rs)
- [`disk_cache/tests.rs`](../aria2-core/src/filesystem/disk_cache/tests.rs)
- [`disk_writer/buffered.rs`](../aria2-core/src/filesystem/disk_writer/buffered.rs)
- [`bt_piece_downloader/multi_file_writer.rs`](../aria2-core/src/engine/bt_piece_downloader/multi_file_writer.rs)

## 2. 数据路径：`Bytes` 降低复制和分配

### 原版差异

原版网络接收、协议解码、Block/Piece 组装和磁盘写入之间通常会经过多个临时
`vector` 或重新分配的拥有式缓冲。乱序 Block、Piece hash 和多文件边界会放大这些
转换成本。

### Rust 实现

- [`SeekableDiskWriter::write_bytes_at`](../aria2-core/src/filesystem/disk_writer/mod.rs)
  允许调用方直接转移 `bytes::Bytes`。
- [`WrDiskCache`](../aria2-core/src/filesystem/disk_cache/) 以 `Bytes` 保存 cache entry，
  重叠范围保留左右片段时使用 `Bytes::slice`，不复制底层 payload。
- BT Piece 校验完成后把拥有的 `Vec<u8>` 转为 `Bytes`，单文件写入直接交给 positioned
  writer；多文件 piece 使用 `Bytes::slice` 跨物理文件切分。
- HTTP 并发 segment 的写入消息也携带拥有的 `Bytes`，让网络执行器和写入器之间
  不需要再复制一份同样的 payload。

### 真实边界

这是一条 reduced-copy path，不是端到端 zero-copy 保证。`write_at(&[u8])` 兼容接口
仍可能创建 `Bytes`，协议解码和 Piece 组装仍可能分配；需要拼接相邻范围时
`BytesMut` 也会产生一次新 buffer。文档和 benchmark 应按“减少复制、减少临时分配”
表述，不应声称 Socket 到磁盘完全零拷贝。

### 证据入口

- [`disk_writer/mod.rs`](../aria2-core/src/filesystem/disk_writer/mod.rs)
- [`disk_cache/write_path.rs`](../aria2-core/src/filesystem/disk_cache/write_path.rs)
- [`bt_download_execute/execute/piece_download.rs`](../aria2-core/src/engine/bt_download_execute/execute/piece_download.rs)
- [`bt_piece_downloader/multi_file_writer.rs`](../aria2-core/src/engine/bt_piece_downloader/multi_file_writer.rs)

## 3. Hash 校验：有界后台 worker 和可取消完整性检查

### 原版差异

当大 Piece 或完整文件下载完成时，如果 hash 直接运行在当前 engine / I/O 执行流，
CPU 密集计算会延迟网络事件、RPC 和其他下载任务。

### Rust 实现

- [`Checksum::verify_async`](../aria2-core/src/checksum/checksum.rs) 使用共享
  `Semaphore` 和 `spawn_blocking`；worker 数按 CPU 数计算并限制在最多 4 个。
- [`CheckIntegrityMan`](../aria2-core/src/checksum/check_integrity/man.rs) 以后台 worker
  处理 Piece 和 whole-file integrity task。大文件按 chunk 推进，每个 chunk 后
  `yield_now`，并通过 RequestGroup 生命周期处理 pause、remove、halt 和取消。
- HTTP、Metalink、FTP、SFTP 的 whole-file 校验统一进入
  `enqueue_file_checksum_for_group`；BT Piece hash 校验也通过异步 hash helper 运行，
  让已下载的 payload 所有权回到写入路径而不再额外 clone。

### 兼容性边界

hash 算法、结果和失败 Piece 集合保持原有语义；改变的是执行位置、并发上限和取消
时机。默认完整性 dispatcher 仍按顺序消费任务，以保留原版生命周期语义，因此
“后台执行”不等于无限并发。

### 证据入口

- [`checksum.rs`](../aria2-core/src/checksum/checksum.rs)
- [`check_integrity/man.rs`](../aria2-core/src/checksum/check_integrity/man.rs)
- [`bt_download_execute/execute/piece_download.rs`](../aria2-core/src/engine/bt_download_execute/execute/piece_download.rs)
- `cargo test -p aria2-core --all-features --lib checksum::check_integrity::man::tests`

## 4. BitTorrent 和 DHT：降低重复扫描并限制并发

### BitTorrent peer、piece 和 HAVE

- [`DefaultPeerStorage`](../aria2-core/src/engine/bt_peer_storage/) 使用
  `HashSet` 做 endpoint 去重和 connected-peer identity 管理，`VecDeque` 保持 unused
  和 dropped peer 的 FIFO 生命周期，减少重复 peer 插入和无界增长。
- [`PeerBitfieldTracker`](../aria2-protocol/src/bittorrent/piece/peer_tracker.rs)
  用 peer `HashMap` 保存 bitfield，并维护每个 Piece 的 peer frequency；rarest-first
  不必在每次选择时重新扫描全部 peer 的完整 bitfield。
- `broadcast_have` 先序列化一次 HAVE frame，再通过
  `for_each_concurrent(64)` 向 active peers 发送。它仍然需要向每个 peer 发消息，
  但避免了每个 peer 重复编码，并把并发写入限制在固定窗口内。
- BT piece loop 使用有界 block pipeline 和并发 endgame response 消费，避免单个慢
  peer 阻塞整个 piece 的响应收集。

### DHT routing 和 UDP 入站

- [`BucketTreeNode`](../aria2-protocol/src/bittorrent/dht/bucket_tree.rs) 用二叉树
  表达 Kademlia bucket range，按 ID 查找 bucket；closest-node 选择使用固定大小
  top-K `BinaryHeap`，选择步骤从完整排序降为 `O(N log K)`，并只保留 K 个候选节点。
- [`engine_inner.rs`](../aria2-protocol/src/bittorrent/dht/engine_inner.rs) 使用容量
  1024 的入站队列和 4 个 worker 处理 KRPC decode、query handler 和 response，队列满
  时显式丢弃报文而不是无限堆积。
- [`DhtTaskExecutor`](../aria2-protocol/src/bittorrent/dht/task.rs) 提供 semaphore
  限制的任务执行器和 periodic-1、periodic-2、immediate 三条 lane，避免维护任务
  吞掉所有即时查询配额。

### 当前边界

入站 worker、routing tree 和任务执行器已有代码及测试；`DhtEngine` 的部分周期维护
路径仍直接调用 `refresh_buckets` / `contact_nodes`，尚未全部切换到上述 task queue。
因此该项应标记为“并发基础设施已实现、完整调度接线仍有边界”，不能宣称所有 DHT
请求都已经通过统一任务队列调度。

### 兼容性边界和证据入口

KRPC、BitTorrent message 格式和 HAVE 的外部语义不变；变化是候选选择、队列容量和
发送并发度。相关入口：

- [`bt_peer_storage`](../aria2-core/src/engine/bt_peer_storage/)
- [`bt_peer_interaction/mod.rs`](../aria2-core/src/engine/bt_peer_interaction/mod.rs)
- [`piece/peer_tracker.rs`](../aria2-protocol/src/bittorrent/piece/peer_tracker.rs)
- [`dht/bucket_tree.rs`](../aria2-protocol/src/bittorrent/dht/bucket_tree.rs)
- [`dht/engine_inner.rs`](../aria2-protocol/src/bittorrent/dht/engine_inner.rs)
- [`dht/task.rs`](../aria2-protocol/src/bittorrent/dht/task.rs)

## 5. 文件预分配：平台适配和可取消 fallback

### Rust 实现

[`FileAllocationMan`](../aria2-core/src/filesystem/file_allocation_man.rs) 以后台
worker、FIFO queue、`Notify` 和 completion channel 管理分配任务。`Prealloc` 只有在
原生分配不可用时才进入复用 zero buffer 的异步 zero-fill，并在 1 MiB chunk 之间
协作式让出；engine halt 会清理队列和 waiter。

| 平台 | 实现 | 失败或权限边界 |
| --- | --- | --- |
| Linux | raw `fallocate(2)`，显式识别 `EOPNOTSUPP`，再对新增区域做 async zero-fill。 | 不支持 fallocate 的文件系统退回 cooperative zero-fill；没有 raw fd 时退回 `set_len`。 |
| macOS | `fcntl(F_PREALLOCATE)`，先保证文件长度；`secure-falloc` 决定是否再 zero-fill。 | `F_PREALLOCATE` 失败时保留正确但可能 sparse 的 `set_len` 结果。 |
| Windows | 先 `SetEndOfFile`，再尝试启用 `SE_MANAGE_VOLUME_PRIVILEGE` 并调用 `SetFileValidData`。 | 没有权限或调用失败时保留 sparse 文件，并记录 fallback；secure 模式再 zero-fill。 |
| 其他 Unix | 使用可移植的 `set_len` 路径。 | 没有统一的 native preallocation API。 |

所有可能等待文件系统或权限管理器的 native call 都不在 Tokio reactor 上执行。
`SetFileValidData` 和 `F_PREALLOCATE` 本身不负责清零时，安全模式会付出额外 I/O；
这属于明确的安全/性能 trade-off，不应隐藏在静默 fallback 中。

### 兼容性边界和证据入口

分配策略名、resume 语义、已有 partial payload 保留规则和文件最终长度保持不变；
平台差异只影响分配速度、sparse 状态和安全清零成本。入口和测试：

- [`file_allocation/falloc.rs`](../aria2-core/src/filesystem/file_allocation/falloc.rs)
- [`file_allocation/windows.rs`](../aria2-core/src/filesystem/file_allocation/windows.rs)
- [`file_allocation/strategies.rs`](../aria2-core/src/filesystem/file_allocation/strategies.rs)
- [`file_allocation_man.rs`](../aria2-core/src/filesystem/file_allocation_man.rs)
- `cargo test -p aria2-core --all-features --lib filesystem::file_allocation`
- `cargo test -p aria2-core --all-features --lib filesystem::file_allocation_man`

当前主要验证来自 Windows 工作树；Linux/macOS 的 syscall、权限和文件系统组合仍
需要 CI 或真实目标机验证。

## 6. RPC：owned parsing、只读 batch 并发和 mutation barrier

### Rust 实现

- [`parse_aria2_wire_document`](../aria2-rpc/src/json_rpc.rs) 使用 serde visitor 解析
  wire document，并直接产生拥有的 request；HTTP/WebSocket 进入
  `handle_request_owned` 后不再为 batch dispatch 复制整棵 parameter DOM。
- [`dispatch_wire_entries`](../aria2-rpc/src/engine.rs) 将连续只读方法分组，以最多
  64 个并发 future 执行，并保持 response 的输入顺序。
- 当同一 wire batch 同时包含两个以上 `tellActive`、`tellWaiting`、`tellStopped`
  或 `getGlobalStat` 请求时，批内共享一次 `RpcReadSnapshot`；这避免热门 WebUI
  轮询对同一批任务反复遍历和构造状态对象。单请求仍使用实时 handler，避免把
  跨请求缓存误当成一致性协议。
- `system.multicall` 中如果全部子调用都是只读操作，并且包含至少两个上述状态
  方法，也共享一次 `RpcReadSnapshot`；这是 WebUI 常用轮询形状。只要 multicall
  含有生命周期 mutation，就关闭快照并按子调用顺序读取状态，保持原版可观察语义。
- mutation 是顺序 barrier：前一段只读请求先 drain，`add/pause/remove/changeOption`
  等修改操作按输入顺序执行，之后才开始下一段只读请求。这样常见的 WebUI polling
  batch 可以并发读取，而生命周期状态不会被重排。
- base64 payload decode、Metalink XML/graph conversion 等 CPU 或大输入处理通过
  `spawn_blocking` 执行。单请求 `addTorrent`/`addMetalink` 先完成这些准备工作，
  再进入 mutation gate；gate 只覆盖 GID/RequestGroup 注册、队列提交和位置变更，
  因此大 payload 不会阻塞其他 mutation 的最终提交。`addTorrent` 解码后的完整
  buffer 只用于生成短 torrent URI，不会被带入提交阶段；`add_task` 仍先注册
  RequestGroup，再把生命周期命令发送到 core engine command channel。
- `system.multicall` 保持原版的子调用顺序、错误形状和成功结果包装；只读子调用
  仅共享一次状态快照，不改变子调用执行顺序。并发优化主要针对 HTTP/WebSocket
  的 JSON-RPC wire batch。

### 兼容性边界

JSON-RPC/XML-RPC/WebSocket 的方法名、认证、错误码、response 顺序和
`system.multicall` wire shape 是外部契约。只读请求的内部并发不会改变这些字段；
大输入准备可以与其他连接的准备阶段重叠，但最终 mutation commit 仍通过 gate
串行化，避免 RequestGroup 生命周期状态重排。只读 `system.multicall` 不需要取得
mutation gate，但仍按原顺序执行子调用；包含 mutation 的 multicall 才在一个 gate
临界区内执行。

### 证据入口

- [`json_rpc.rs`](../aria2-rpc/src/json_rpc.rs)
- [`engine.rs`](../aria2-rpc/src/engine.rs)
- [`handlers/bittorrent.rs`](../aria2-rpc/src/handlers/bittorrent.rs)
- [`handlers/task.rs`](../aria2-rpc/src/handlers/task.rs)
- [`server/http_routes.rs`](../aria2-rpc/src/server/http_routes.rs)
- [`server/ws_session.rs`](../aria2-rpc/src/server/ws_session.rs)
- [`handlers/handler_tests.rs`](../aria2-rpc/src/handlers/handler_tests.rs)
- `engine::tests::test_mutation_gate_blocks_lifecycle_commit_until_released`

## 7. 既有事件驱动优化

除了上述六类差异，当前 Rust 热路径还包含以下已经在 README 中记录的改进：

- uTP 接收等待 Tokio UDP readiness，并在持久缓冲中保留分片帧。
- BT piece 使用有界 event-driven block pipeline；endgame response 并发消费，
  TCP/MSE 取消后保留未完成帧。
- 令牌桶限速修改会立即唤醒等待者；PieceStat、missing-piece 和 rarest-first
  使用字节扫描或排序游标，减少重复线性扫描。
- engine idle wait 只等待 command、task completion、最早 maintenance deadline
  和 shutdown；session/control-file 保存使用统一 deadline coordinator。

实现入口和测量说明见 [`engine-loop-performance.md`](engine-loop-performance.md)。

## 验证和性能声明边界

当前证据分为三层：

1. **正确性和生命周期**：core、protocol、RPC 的 focused unit/integration/E2E tests
   覆盖 positioned write、cache flush、hash cancellation、BT/DHT handler、file
   allocation fallback、RPC batch 和 wire compatibility。
2. **Rust 内部回归**：Windows release 工作树上的 Criterion 和 engine-loop workload
   可用于比较同一 Rust 实现优化前后。README 中的 bitfield 数值和
   [`engine-loop-performance.md`](engine-loop-performance.md) 都属于这一层。
3. **跨实现性能**：尚未完成与 `aria2_original` C++ binary 的同工作负载、同磁盘、同
   网络条件对比，也没有完成真实 2 GB/s 下载端到端报告。因此当前不能从这些结果
推导“整体吞吐优于原版”或固定百分比收益。

### 2026-08-28 Windows positioned-write measurement

Using the single default Cargo target on the current Windows host:

```text
cargo bench -j 1 -p aria2-core --all-features --bench positioned_write_bench -- --warm-up-time 1 --measurement-time 2 --sample-size 10
```

The lifecycle-matched `positioned_write/PositionedDiskWriter_concurrent_4x1MB`
case measured `1.7509 GiB/s` at the midpoint (`[1.7364, 1.7626] GiB/s`), while
`OldMutexDirectDiskAdaptor_4x1MB` measured `2.2850 GiB/s` at the midpoint
(`[2.2126, 2.3525] GiB/s`). Both paths open and pre-size their handles before
the measured iterations; only the four writes and final flush are measured.
This is a Rust-internal comparison, not an aria2_original comparison, and it
does not establish a performance win. On this Windows host the positioned
path is about 23% slower in this workload, so the result is a real regression
signal requiring profiling and optimization before any performance claim is
made.

The fixed 10 MiB/16-segment regression workload separately completed at
`562.5 MiB/s` (`17.7785 ms`) and remains a correctness/threshold check rather
than a cross-implementation baseline.

复现已有 Rust-only 微基准：

```bash
cargo bench -p aria2-core --bench segment_scan_bench -- --noplot
cargo bench -p aria2-protocol --features bittorrent --bench sequential_picker_bench -- rarest_selection --noplot
cargo bench -p aria2-rpc --bench rpc_bench -- read_only_multicall_poll_100_tasks --noplot
```

最终兼容性状态仍以 [`compatibility-status.md`](compatibility-status.md) 为准；本台账
只描述内部性能差异和它们目前的证据范围。
