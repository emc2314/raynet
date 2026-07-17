# RayNet 设计文档

本文档定义 RayNet 的目标架构、协议规格和关键权衡。

## 简洁性第一：核心设计原则

RayNet 的首要设计目标是简洁、统一和优美。后续设计出现冲突时，优先选择状态更少、分支更少、契约更清晰的方案。

这个原则直接带来以下约束：

1. **单版本部署。** 同一部署中的所有 core、endpoint、relay 和相关 shell 运行完全相同的协议版本。Wire format、配置格式、状态机和算法按当前版本定义，升级以全部节点原子替换为目标。
2. **契约驱动。** Shell 保证传给 core 的配置、时间、event、`ConvId`、buffer 大小、调用顺序和 action 回灌满足 API 契约。Core 直接依赖这些前置条件。
3. **Fail-fast。** 已通过不可信输入边界并进入 core 状态机的数据满足已声明的不变量。内部不变量破坏、不可达状态和 Shell 契约违约通过普通索引、`unwrap`、`assert!` 或 `unreachable!` 立即终止进程。
4. **结构性解决复杂性。** Bug 暴露设计复杂性时，协议或状态机重构为更简洁的模型；功能需求与简洁模型冲突时，目标能力收缩到能够以统一规则完整实现的范围。

`TransportPacketReceived.bytes`、解密后的可变长 wire 字段和远端状态属于不可信协议输入。认证、长度、边界、freshness、replay 和 MTU 检查构成协议正确性与内存安全边界；非法网络输入安全 drop。经过认证、且只能由同版本可信 endpoint 产生的控制不变量可以使用 fail-fast 语义。

Core 使用单一同步状态机，保持通用的 session 和 packet 语义；SOCKS5、HTTP CONNECT、host/port 和 L2/L3 能力属于 Shell。协议只包含当前功能所需的状态和扩展点。

## 1. 目标与边界

RayNet 是由 Rust core 和 runtime shell 组成的 overlay network。

Core 是同步状态机，负责协议状态、加密、KCP session、relay forwarding 和单向 route plan 调度。Shell 负责真实 IO、timer、配置加载、启动熵、日志、ingress/egress、代理协议处理和 transport channel 连接。

1. Entry endpoint 的 shell 接收本地连接，并请求 core 创建 endpoint session；core 分配 `ConvId` 并回传给 shell。
2. Endpoint 发送 packet 时使用一个单向 `RoutePlan`。
3. Relay 消费 `RoutePlan` 的第一项并转发；endpoint message 始终由 endpoint 处理。
4. Exit endpoint 的 shell 按 core 输出的 `OpenSession` 创建本地 session handler。目标地址、握手和应用协议都属于 shell，不属于 core。

传输 channel 可以是 UDP、TCP、HTTP、DNS、mailbox/file、GitHub upload/download 或其它 shell 能实现的东西。RayNet 通用配置不定义 channel kind。`ChannelId` 只是某个节点本地的 transport handle。

Channel 可以是有向的；`A -> B` 存在不代表 `B -> A` 存在。Entry -> exit 和 exit -> entry 是两个独立方向，可以使用不同 route、不同 MTU 和不同健康状态。

Relay 和 endpoint 的 channel table 在进程生命周期内固定。增删或重编号 channel 需要重启相关节点，并同步相关 endpoint 的 route 配置。

Core 只推进内存中的协议状态。Shell 把外部事件转成 `CoreEvent`，core 推进状态并输出 `CoreAction`，shell 执行动作。IO、系统时间、task、sleep、socket、文件、环境变量、网络 API 和系统随机源都属于 shell。

Core 从构造配置里的 128-bit `random_seed` 获得随机性。Shell 每次启动生成均匀随机的 seed。Core 用该 seed 初始化一个单向共享 DRBG；同一实例内所有随机消费顺序读取这条伪随机字节流。

## 2. 分层与职责

```text
Runtime Shell
  Tokio shell
  future shells (C, Python, BEAM VM)
  ingress / egress
  proxy protocol handling
  transport channel connections
  management / config / logging

RayNet Core
  sync state machine
  opaque transport packet generation/parsing
  envelope authentication and encryption
  endpoint message encryption
  KCP session
  route-plan forwarding
  channel health and scheduling
```

Core 只定义 session、transport packet 和同步状态机边界，不定义 SOCKS5、HTTP CONNECT、TUN/TAP、UDP socket、HTTP client 或 mailbox 等 shell 语义。Shell 内部可以用 plugin 或其它模块化方式组织 proxy 和 channel 实现，但这不是 core API 或 wire protocol 的一部分。

## 3. 节点角色

```text
Entry Endpoint
  ingress connection
  endpoint session
  outbound route planning
  route health learning

Relay
  envelope authentication/decryption
  route-plan consumption
  local channel forwarding

Exit Endpoint
  exit connection
  endpoint session
  outbound route planning
  route health learning
```

Entry 和 exit 是 shell 对本地连接用途的描述，不是 Core role。`EndpointCore` 没有 entry/exit 配置；同一个实例可以同时包含本地发起和远端发起的 session。`SessionOpen` 使该 session 成为本地发起方，收到远端 Open 使其成为远端发起方，后续状态机只依赖这个 per-session 事实。Shell 决定哪些 ingress 可以提交 `SessionOpen`，以及 `OpenSession` action 应创建哪种 egress handler。

Relay 的行为固定为：认证解开 packet，消费 `route_plan[0]`，按指定本地 channel 发出。

## 4. Core API

Core 提供 Rust-native API。跨语言 shell 通过 Rust core 的 API/ABI 驱动同一个状态机。对外配置和标量尽量使用 `u8`、`u16`、`u32`、`u64` 与零值约定；Rust 实现进入 core 后再转换成 `Option<NonZeroU32>`、枚举和固定点数等内部类型。Slice、owned buffer、enum tag 和 action sink 由未来的 C ABI adapter 展开为 pointer、length、tag 与 callback，不反向限制 core 内部表示。

```text
EndpointCore::new(config) -> EndpointCore
RelayCore::new(config) -> RelayCore

core.handle_event(elapsed_ms, CoreEvent, action_sink) -> CoreEventResult
core.poll(elapsed_ms, action_sink)
core.next_deadline(elapsed_ms) -> u64
core.metrics() -> CoreMetrics
```

`core` 表示 `EndpointCore` 或 `RelayCore`；两者共享完全相同的 event/action 驱动方法签名。

`elapsed_ms` 和 deadline 都是 `u64`，表示 shell 单调时钟相对于 core 启动时刻的毫秒数，用于本进程 timer 和调度。`next_deadline` 返回 `u64::MAX` 表示当前没有 deadline。Shell 在收到外部事件时调用 `handle_event`，在 deadline 到期时调用 `poll`。`action_sink` 是 core 写入输出动作的临时 sink；Rust 实现可以直接使用 `&mut Vec<CoreAction>`。Sink 只收集数据，不在 core 调用栈或 core 锁内回调 shell，避免 action 执行时重入 core。

没有 deadline 是正常空闲状态。Relay 没有周期任务时只由 transport event 驱动；Endpoint 在 KeepAlive 关闭，且没有 KCP 待发送/重传、outstanding feedback、关闭期限或待清理状态时同样没有 deadline。Feedback interval 到期只使下一份实际 outbound packet 成为样本，不单独产生 packet 或 poll deadline。

所有 core API 都是同步状态机调用。`handle_event` 对大部分 event 返回 `CoreEventResult::None`；EndpointCore 处理 `CoreEvent::SessionOpen` 时，在同一次调用里分配 `ConvId` 并返回 `CoreEventResult::SessionCreated { conv_id }`。处理 `CoreEvent::SessionWrite` 时，core 要么完整接收本次 bytes 并返回 `None`，要么完全不接收并返回 `SessionWriteBlocked`；不进行部分写入。RelayCore 的所有合法 event 都返回 `None`。

Shell 的 action 执行循环遵循固定顺序：

1. 每次 core 调用使用独立的临时 `actions` 容器。
2. Shell 调用 `handle_event` 并取得 `CoreEventResult`。
3. 若结果是 `SessionCreated { conv_id }`，先把该 `conv_id` 绑定到触发本次调用的本地连接。
4. `SessionWrite` 返回 `None` 时，本次 bytes 已被完整接收，shell 继续读取该本地连接。
5. 若结果是 `SessionWriteBlocked`，shell 保留原 buffer、停止读取新数据并原样重试，直到返回 `None` 或 session 关闭。
6. Result 处理和 action 投递都发生在 core 调用返回之后，不重入 core。Action 的异步执行结果通过后续 `CoreEvent` 反馈。

多个连接可以并发到达 shell，但对单个 core 实例的调用按同步状态机顺序串行化。每个 `SessionOpen` event 都有自己的调用栈、result 和 actions，因此不需要 request id，也不会把不同连接的 `ConvId` 混在一起。

Rust API 的 event 来自可信 shell。Session event 只提交给 `EndpointCore`；调用顺序、`ConvId`、flow control 和其它前置条件由 shell 保证。契约违约使用 `unreachable!`、`assert!`、`expect`、索引检查或状态机不变量立即 panic。跨语言 ABI adapter 把外部 tag 和长度转换成合法的 Rust enum/value 后再进入协议状态机。

网络 packet 属于不可信输入。认证失败、格式错误、过期、重放、未知远端 session 或非法远端状态在 core 内安全 drop 并更新 metrics。

```text
CoreEventResult
  None
  SessionCreated { conv_id }
  SessionWriteBlocked
```

运行期状态通过同步 result、状态机和 action 表达。暂时没有可用 route 时，已经接收的 Open/Data 留在有界 KCP 队列等待 channel/route 恢复；本地发送失败通过 `TransportPacketSendFailed` 回灌。KCP 队列没有空间完整容纳一次 `SessionWrite` 时返回 `SessionWriteBlocked`；Core 不保留输入 buffer，也不产生后续 writable 通知。Shell 保证每次 `SessionWrite` 不超过 `MAX_SESSION_DATA_SIZE`，并持有被拒绝的原 buffer 负责等待和重试。

配置文件解析、操作者诊断和拓扑校验属于 shell。Core 构造器接收满足契约的配置并直接返回实例；runtime API 返回状态机结果。

Core config 在构造时一次性传入，并在实例生命周期内保持固定。

```text
EndpointConfig
  envelope_key
  message_key
  random_seed: [u8; 16]
  boot_time_ms: u64
  kcp: KcpConfig
  route: RouteConfig
  local_channels: Vec<ChannelId>
  padding_reserve: u8
  keepalive_interval_ms: u32

RelayConfig
  envelope_key
  random_seed: [u8; 16]
  boot_time_ms: u64
  local_channels: Vec<ChannelId>
  local_min_mtu: u32

KcpConfig
  send_window: u16
  receive_window: u16
  time_scale: u8
  fast_resend: u32

RouteConfig
  graph: RouteGraph
  min_mtu: u32
  feedback_interval_ms: u32
  feedback_timeout_ms: u32
```

`boot_time_ms` 是 shell 在 core 启动时采样的 `GlobalTime`，不是操作系统的启动时间。运行期间 core 用它加上 `elapsed_ms` 得到当前 `GlobalTime`。任意通信节点之间的 `GlobalTime` 目标差值不超过约一分钟。

`KcpConfig` 只属于 endpoint。两个方向的 endpoint 可以使用不同窗口、`time_scale` 和 fast-resend threshold；`fast_resend = 0` 表示关闭 fast retransmit。Shell 根据各自发送方向的带宽、延迟离散程度和丢包特征提供有效提示，0 代表使用 Core 默认值。MTU、窗口和 timing 在 KCP session 构造时一次确定，并在该 session 生命周期内保持固定。

`padding_reserve` 由 shell 提供，范围 `1..=255`。示例 shell 使用 `128`。它只影响本 endpoint 发送方向的 KCP MTU 计算，不需要对端协商。`route.min_mtu` 是该发送方向整张 graph 中所有 channel 共同保证的 transport packet MTU 下限；`local_min_mtu` 是 relay 本节点所有 outbound channel 的保守最小 MTU。两者可以大于协议包长上限；Core 计算 KCP MTU、padding 和重新封装空间时使用 `min(configured_mtu, MAX_TRANSPORT_PACKET_SIZE)`，不生成 wire 无法承载的 packet。

KeepAlive 和 route feedback 是两个正交语义，但共享同一种 endpoint packet 反馈机制。KeepAlive 是 endpoint-level 的单向空消息：某一端在 `keepalive_interval_ms` 内没有发送其它 endpoint packet 时可以发送一份 KeepAlive；它和其它 endpoint packet 一样刷新 sequence window，对端不因为“它是 KeepAlive”而回复。两个方向独立配置，可以只启用其中一个方向；`keepalive_interval_ms = 0` 表示本 endpoint 不产生空闲 KeepAlive。`feedback_interval_ms` 和 `feedback_timeout_ms` 的配置契约要求大于零。

每份 endpoint packet 都有加密的 2-bit `reply_depth`。`KcpPacket` 或 `KeepAlive` 可以用深度 1 发起 feedback chain；`RouteReply` 在自己也需要采样反向 route 时，可以用深度 2 或 3 继续同一条 chain。深度 0 不请求回复，深度 3 的回复回到 0，因此一条 chain 有固定上界。每个 endpoint 按自己的 `route.feedback_interval_ms` 独立决定何时发起或继续采样；长 RTT 只增加并发 outstanding 数量，不阻塞后续 route 的采样。

KeepAlive interval、`route.feedback_interval_ms`、随机 jitter 和 `route.feedback_timeout_ms` 都直接使用 `elapsed_ms`，不受 KCP `time_scale` 影响。活跃业务 packet 持续提供 route 样本；完全空闲时，KeepAlive 是否启用决定 route health 是否继续更新。

密钥模型区分两类 key：

```text
envelope_key
  用于 envelope 认证加密
  EndpointCore 和 RelayCore 都持有

message_key
  用于 endpoint message 端到端认证加密
  只有 EndpointCore 持有
```

生产部署应让 envelope key 与 message key 分离。同一 key 兼用于两层 AEAD 会削弱边界，也不利于按层轮换。

`RouteConfig` 描述本 endpoint 发送方向可见的拓扑、edge 调度 hint 和 feedback timing。`RouteGraph` 是按拓扑顺序存储的有向无环图；core 直接在整张图上为每个 endpoint packet 按需生成实际 `RoutePlan`，拓扑中的平行、共享、分叉和汇合关系都由 graph 本身表达。

```text
RouteGraph
  nodes: Vec<RouteNode>

RouteNode
  edges: Vec<RouteEdge>

RouteEdge
  channel_id
  next: u32
  capacity_hint_kbps: u32
  latency_hint_ms: u32
```

`nodes[0]` 固定表示本 endpoint，`nodes[n - 1]` 固定表示目标 endpoint。每条边的 `next` 都是 `nodes` 的下标，并且必须大于当前节点下标；目标节点没有 outgoing edge。数组顺序同时表达 source、destination 和 DAG 拓扑。

同一节点内的 `ChannelId` 唯一；不同节点可以使用相同数值。允许两个节点之间存在多条使用不同 `ChannelId` 的平行边。图中的每个节点都必须从 source 可达，并且可以到达 destination。图的节点数和最长路径必须满足编译期 route-plan 长度上限。

`capacity_hint_kbps` 和 `latency_hint_ms` 是 edge 正常状态的调度先验，不是实时 transport 状态。`Kbps` 表示十进制 kilobits per second，即 `1 Kbps = 1_000 bits/s`；非零 `u32` 可表示从 1 Kbps 到约 4.29 Tbps。非零值分别近似物理服务速率和单向固定传播/处理延迟；channel 状况变化时允许它们偏离真实值。值为零时由 Core 分别替换为协议常量 `DEFAULT_ROUTE_CAPACITY_KBPS = 50_000` 和 `DEFAULT_ROUTE_LATENCY_MS = 50`，因此 Shell 可以把未配置的 hint 原样传为零。Core 根据整个 graph 中共享的 edge 自动计算并行连接、共享下游瓶颈和分叉汇合结构反向通信使用另一份独立的 `RouteConfig`。

`RouteConfig.min_mtu` 使同一发送方向的所有 route 共享一个固定 KCP MSS，任意 KCP packet 都能在不重分段的情况下换 route 重传。实际 KCP MTU 受 `MAX_TRANSPORT_PACKET_SIZE` 封顶。MTU 是整张 graph 的保守契约，不作为 per-edge hint；各物理 channel 可以支持更大 MTU，但超过协议包长上限的额外容量不扩大 RayNet wire packet。

`RoutePlan` 是 `Vec<ChannelId>`，由 endpoint core 从 `RouteGraph` 生成。一个 plan 描述某个 packet 当前方向要走的本地 channel 序列。Endpoint 按 edge 的虚拟服务时间和连续 `loss_ewma` 计算预计最早抵达路径，relay 按 plan 指定的本地 `ChannelId` 转发。

面向人的 shell 配置可以用任意局部名称引用节点。Shell 负责解析名称和拓扑顺序，并把可选 hint 转成固定宽度整数；core 接收稠密 `RouteConfig`，归一化零值后直接依赖其结构满足配置契约。两个方向的 graph 之间没有共享的节点标识或运行时状态。

Core 不产生面向用户的诊断文本。配置解析和操作者诊断属于 shell；网络协议丢弃原因和统计使用数值或结构化类型，shell 根据部署环境渲染日志。非测试构建中的 core 不包含项目自有的 `expect`/`panic` 文本或泄露秘密状态的 Debug。

## 5. 标识

```text
ChannelId
  u16
  某个节点本地的 transport channel 编号
  在 route plan 中按当前节点的本地语义解释

ConvId
  u64
  endpoint session 编号，也是该 session 的 KCP conv
  由发起侧 EndpointCore 分配
  一个 session 对应 shell 暴露给 core 的一条逻辑连接
  进入 KCP segment，并受 endpoint message 加密保护

SeqId
  u48
  发送 EndpointCore 实例的随机编号
  启动时从 random stream 派生，并在该实例生命周期内保持不变

SeqNo
  u48
  SeqId 内单调递增的 packet 序号
```

Rust Core 内部使用 `u64` 保存 `SeqId` 和 `SeqNo`，并保证值小于 `2^48`；`u48` 只描述 wire 宽度。

Socket、文件描述符、HTTP stream、平台连接句柄或其它本地连接资源都属于 shell。Shell 自己维护 `ConvId` 到本地连接资源的映射。

`ConvId` 在两个 endpoint 之间标识 KCP session。Shell 在 entry 和 exit 两端都能看到它，并用它把 core session 绑定到本地连接资源。EndpointCore 从随机流读取随机起点，随后用递增的 `u64` counter 分配 id。这个 namespace 让 entry 单独重启而 exit 保留旧 session 时仍具有足够低的碰撞概率。

## 6. CoreEvent

```text
CoreEvent
  TransportPacketReceived {
    channel_id,
    bytes
  }
  TransportPacketSendFailed {
    channel_id,
    bytes
  }
  SessionOpen
  SessionWrite {
    conv_id,
    bytes
  }
  SessionClose {
    conv_id,
    reason
  }
```

`CoreEvent` 是 shell 提交给 core 的输入。它把 transport 收包与发送结果、本地 session open、write 和 close 输入送入同步状态机。`SessionOpen` 的 `ConvId` 通过本次 `handle_event` 的同步 `SessionCreated` result 返回。Session event 只允许提交给 `EndpointCore`；把它提交给 `RelayCore` 是 shell bug。

`SessionWrite` 返回 `None` 表示完整 bytes 已被接收；返回 `SessionWriteBlocked` 表示任何 byte 都未被接收，输入内容保持不变。Core API 不规定 buffer 的具体表示或跨边界所有权机制。

Shell 确认 EndpointCore 产生的 outbound packet 在本地发送失败时，把原始 action bytes 通过 `TransportPacketSendFailed` 交还给该 EndpointCore。Endpoint 把本地 first edge 置入短 cooldown，并为同一个 KCP packet 重新生成 envelope 和完整 route。Relay 的发送 action 失败时由 shell 结束该 packet 并记录 shell transport metric；端到端 KCP 负责恢复。RoutePlan 和 endpoint message 由发送 endpoint 决定，Relay 保持 endpoint payload opaque。

KCP timeout 指某个未确认 segment 的 RTO 到期并触发重传。它属于 KCP 的可靠传输逻辑，不直接进入 route feedback 或 edge health：同一 segment 的不同发送可能使用不同 `RoutePlan`，而一次 KCP packet 又可能聚合多个 segment，无法从 KCP timeout 精确推出某条 route 是否健康。KCP 持续无进展可以把下一次 route feedback request 提前，但只有本地发送结果、`RouteReply` 或 reply deadline 才更新 edge health。Route feedback 见第 8.3 和第 11.2 节。

## 7. CoreAction

```text
CoreAction
  SendTransportPacket {
    channel_id,
    bytes
  }
  OpenSession {
    conv_id
  }
  WriteSession {
    conv_id,
    bytes
  }
  CloseSession {
    conv_id,
    reason
  }
```

`CoreAction` 是 core 返回给 shell 的输出。Shell 按 action 执行 transport 发送、创建远端 session handler、写入本地连接或关闭本地资源。

Shell 执行 `SendTransportPacket` 时必须使用 core 指定的 `channel_id`。Shell 不解释 `bytes`，只把它当 opaque bytes 发给指定 channel；bytes 长度不超过该 channel 的 MTU。

Shell 执行 `OpenSession` 时创建本地 session handler。它表示远端 Open 已可靠到达，不等于目标连接已经建立成功。Shell 负责解析后续 Data 中的私有代理协议并建立真实连接；目标地址、握手、方向性 EOF 等控制语义都由该私有协议表达。本地初始化失败使用 `CoreEvent::SessionClose { reason: Reset }` 回报。

Shell 执行 `WriteSession` 和 `CloseSession` action 时，用自己的 session map 找到真实本地连接资源。Session input 使用名词在前的 `SessionOpen`、`SessionWrite`、`SessionClose`；输出给 shell 的命令使用动词在前的 `OpenSession`、`WriteSession`、`CloseSession`。词序和 enum 类型共同标明数据流方向。

数据路径的目标是每次实际发送只在组装最终 `TransportPacket` 时复制一次 payload，endpoint message 和 envelope 的 AEAD 都在最终 buffer 上原地完成。具体 buffer 类型和内部组织不属于协议接口。

每个 session 的发送积压由 KCP `wait_snd()` 表示，也就是尚未发出的 send queue 与已经发出但尚未确认的 send buffer 中的 segment 总数。Core 以 `send_window` 作为两者不可突破的总容量；只有 `wait_snd() + 本次消息的 fragment_count <= send_window` 时才完整接收 `SessionWrite`。若本地关闭时窗口已满，Close 暂存在 session 状态中，等 ACK 释放一个槽位后再作为最后一条 KCP message 入队；它不突破 `send_window`。

`SessionWriteBlocked` 是正常的同步 flow-control 结果，不是 `CoreError`。Blocked 调用不保留输入 buffer，不改变 session 或 KCP 状态，也不产生 action；对同一个 buffer 的后续重试仍执行同一个容量判断。一个 session 同时至多有一份被拒绝的 bytes 保留在 shell，在它被接收前 shell 不读取下一份数据。等待时机、重试频率和 session 关闭时释放待写 buffer 都属于 shell 的运行时策略。高 RTT、丢包重传、远端接收窗口、拥塞窗口以及暂时没有可用 route 会延长 Blocked 状态，但不会增加 core 的队列上限。

高频不可信输入的丢弃和认证失败不逐包输出 Action。Shell 通过 `core.metrics()` 定期采样累计计数，再转换成日志或指标系统。

## 8. Wire Protocol

```text
TransportPacket
  shell 收发的 opaque bytes
  外部观察者不应看到 magic、明文版本、明文类型或固定协议标记

Envelope
  envelope 认证解开后的节点间转发单元

EndpointMessage
  endpoint 间端到端加密和认证的数据；承载 KCP packet、KeepAlive 或 RouteReply

SessionMessage
  KCP reassembly 后的单连接消息
```

### 8.1 TransportPacket

TransportPacket 固定为：

```text
[0, 16)   envelope_nonce
[16, 32)  envelope_tag
[32, L)   encrypted_envelope
```

Shell 只处理长度不超过 channel MTU 的 opaque bytes。接收方使用 `envelope_key` 认证解密，外观上没有稳定的明文协议标记。

Shell 把一条完整 transport packet 及其实际字节缓冲区交给 core。不可信输入先检查长度上下限；不合法的包直接丢弃并更新 metrics。UDP datagram、文件对象或 mailbox message 自带消息边界；stream channel adapter 使用载体原生消息边界或固定大小 record。

在 key 保密、nonce 不复用且底层密码假设成立的条件下，任意一个 `TransportPacket` 与同长度的均匀随机字节序列在计算上不可区分。外部观察者仍然能够看到 packet 长度、到达时间和传输方向；随机长度 padding 只改变长度分布，不隐藏这些侧信道。

所有整数、长度、时间和索引字段使用 little-endian。实现使用显式 `to_le_bytes` / `from_le_bytes`，不依赖 CPU 原生端序，也不把 buffer 直接 cast 成结构体。这保证 x86_64 与 arm64 看到同一字节序列。

### 8.2 Envelope

Envelope AEAD 认证解密后得到明文：

```text
[0, 4)                         time_and_route: u32 LE
                                 bit[5:0]  = route_plan_len
                                 bit[31:6] = time
[4, 10)                        seq_id: u48 LE
[10, 16)                       seq_no: u48 LE
[16, 16+P)                     payload: EndpointMessage
[16+P, 16+P+2R)                route_plan: R × ChannelId(u16 LE)
[16+P+2R, 16+P+2R+N)           padding: N bytes，每个 byte 都等于 N
```

其中 `1 <= N <= 255`，`0 <= R <= 63`。

这样布局的动机：

1. `seq_id` 和 `seq_no` 组成连续的 12-byte PacketId，固定 header 保持 16 bytes。
2. EndpointMessage 连续，便于端到端处理。
3. Route plan 放在 payload 之后、padding 之前，relay 消费一跳时不必搬移大块 payload。
4. Padding 仍位于 AEAD 加密范围末尾，外观上不暴露长度字段。

`time` 使用 1024ms tick，并在 26 bits 上做 modular freshness comparison；约 2.18 年的 wraparound 不影响分钟级 freshness window。`time`、`seq_id` 和 `seq_no` 由原始发送 endpoint 创建，在整条 route 上保持不变。`route_plan_len` 随 relay 消费 route 而减少。Relay 校验 sequence 并读取 `route_plan`；`payload` 对 relay 是 opaque bytes。

每一跳都重新封装 envelope，并生成新的 128-bit 随机 envelope nonce。发送 endpoint 在 MTU 允许的范围内随机选择 padding。Relay 按 `min(local_min_mtu, MAX_TRANSPORT_PACKET_SIZE)` 重新计算可用空间，并在 `1..=min(255, available)` 内选择新 padding；packet 可以变长或变短，但不得超过本节点本地 MTU 或协议包长上限。

接收方认证解密后读取最后一个 byte 得到 `N`，验证最后 `N` bytes 都等于 `N`，再根据 header 中的 route length 和 padding 边界分离 payload 与 route plan。

### 8.3 EndpointMessage

```text
payload[0, 16)   message_tag
payload[16, P)   message_ciphertext
```

Endpoint message 的 128-bit AEGIS nonce 直接取 Envelope header 的前 16 bytes，并把 `time_and_route` 的低 6 bit 清零：

```text
nonce = header[0..16]
nonce[0] &= !0x3F
```

这样 nonce 与 header 字节同构，只忽略会随跳数变化的 route length。每个 EndpointCore 实例使用新的随机 `seq_id`，`seq_no` 在该实例内单调递增；`time`、`seq_id` 和 `seq_no` 共同构成该实例的 nonce namespace。Shell 每次启动提供新的随机 seed，并保证重启前后的 `GlobalTime` 连续。wire 只携带 16-byte authentication tag。

Endpoint message plaintext 的首 byte 是独立 flags：

```text
flags:
  bit[0] = kcp_packet
  bit[1] = keep_alive
  bit[2] = route_reply
  bit[4:3] = reply_depth
  bit[7:5] = 0

KcpPacket:
[0]       flags = kcp_packet | (reply_depth << 3)
[1, P)    一个完整 KCP packet

KeepAlive:
[0]       flags = keep_alive | (reply_depth << 3)

RouteReply:
[0]       flags = route_reply | (reply_depth << 3)
[1, 7)    received_seq_id: u48 LE
[7, 13)   received_seq_no: u48 LE
```

三个 packet type bit 中恰好有一个为 1。`reply_depth` 是 `0..=3` 的两位无符号整数。`KeepAlive` plaintext 长度恰好为 1，`RouteReply` 长度恰好为 13。

`u48` 字段按 6 个 little-endian bytes 显式编解码。Wire 数据不映射成 Rust struct，因此 Envelope 和 RouteReply 都不为 CPU 原生对齐增加 padding。

`reply_depth = 0` 表示不需要回复。本地用 `KcpPacket` 或 `KeepAlive` 发起新采样时设为 1。Endpoint 收到任意 `reply_depth > 0` 的 packet 后，把承载它的 envelope `(seq_id, seq_no)` 写入一份 `RouteReply`。该 reply 的 depth 由同一条规则生成：

```text
reply_depth = if local_feedback_is_due {
    (received_reply_depth + 1) & 0b11
} else {
    0
}
```

因此新 chain 最长是 `KcpPacket/KeepAlive(1) -> RouteReply(2) -> RouteReply(3) -> RouteReply(0)`。任何一端当前不需要反向样本时都可以提前用 0 终止；收到深度 3 时，上式必然生成深度 0。一份带非零 depth 的 `RouteReply` 同时完成它 body 指向的旧样本，并以自己的外层 envelope PacketId 发起新样本，不增加 RouteReply body 的 wire 长度。

发送 endpoint 为每份 `reply_depth > 0` 的 packet 保存它自己的 PacketId、所用 `RoutePlan` 和 deadline，并消耗本 endpoint 的一次 feedback interval 预算。收到经过 endpoint AEAD 认证的 `RouteReply` 后，按 body 中的 PacketId 匹配并更新该 forward plan；未知、重复或已过期的 PacketId 幂等忽略。`reply_depth = 0` 的 packet 不保留新的 route feedback 状态。

`KeepAlive` 和 `RouteReply` 是 best-effort endpoint control packet，由 endpoint route scheduler 立即发送；选路或本地发送失败时直接丢弃。请求方以 deadline 完成 feedback 生命周期。

一个 KCP packet 可以包含一个或多个完整 KCP segment，每个 segment 自带 `len`；packet 末尾不能有不属于任何 segment 的尾随字节。第一个 segment header 中的 `conv` 就是 RayNet 的 `ConvId`，同一 packet 内所有 segment 的 `conv` 必须相同。

### 8.4 KCP Segment

```text
[0, 8)    conv: u64 LE          // ConvId
[8, 9)    cmd: u8
[9, 10)   frg: u8
[10, 12)  wnd: u16 LE
[12, 16)  ts: u32 LE
[16, 20)  sn: u32 LE
[20, 24)  una: u32 LE
[24, 28)  len: u32 LE
[28, ...) data
```

KCP header 共 28 bytes。`cmd` 使用 KCP 既有命令值：PUSH=81、ACK=82、WASK=83、WINS=84。ACK、WASK 和 WINS 的 `len` 为 `0`；PUSH 的 `len` 是它自己的 data 长度。KCP ACK 只服务可靠传输；route feedback 采样整个 endpoint packet，因此 ACK 是否聚合不影响反馈格式和语义。

### 8.5 SessionMessage

KCP reassembly 后，endpoint 按 KCP message boundary 解析 `SessionMessage`：

```text
SessionMessage
  Open
  Data { bytes }
  Close { reason }
```

Wire 编码：

```text
Open:  [0]
Data:  [1] || bytes
Close: [2] || reason:u8
```

Close reason：

```text
0 LocalClosed
1 RemoteClosed
2 Reset
3 Error
```

空 `Open` 实现最简洁。Open 与后续 Data 都是独立 KCP message；shell 不必等待 Open 的网络确认即可继续提交 Data，因此不增加协议 RTT，只多一个很小的控制消息。目标信息、代理握手或首个应用数据由 shell 编码到后续 Data 中。

`Data` 不需要自己的长度字段。一次 KCP `send` 对应一个 KCP message；KCP 使用 segment 的 `frg` 和 `len` 完成分片、重组并恢复 message boundary。`SessionMessage::Data` 的 bytes 就是重组后该 KCP message 中除去首个 type byte 的全部剩余字节。面向 TCP stream 的 shell 可以顺序写出多个 Data，让边界自然消失；面向 packet 的 shell 可以保留一条 Data 对应一个 packet。Core 不解释 Data 内容。

协议固定：

```text
MAX_SESSION_DATA_SIZE = 65535
```

`MAX_SESSION_DATA_SIZE` 是 Shell 可提交的 bytes 上限。Core 添加 SessionMessage type byte，KCP 继续按 MSS 分片，并满足：

```text
fragment_count = ceil((1 + MAX_SESSION_DATA_SIZE) / mss)
fragment_count < receive_window
fragment_count < 256
fragment_count <= send_window
```

每次 `SessionWrite` 对应一个完整 KCP message。队列空间不足时整个 message 留在 shell；空间足够时整个 message 一次进入 KCP，不跨两次 event 做部分提交。

SessionMessage 的控制类型为 Open 和 Close；应用错误由 shell 的内部协议和固定 Close reason 表达。

### 8.6 威胁模型

Envelope auth 使用认证加密保护 envelope，防止外部伪造或篡改。Message encryption/auth 保护 endpoint message，relay 不持有 endpoint message 的解密能力。

威胁模型信任已认证 relay。Relay 可以看到 route plan，endpoint message 对 relay 保持机密。

外部攻击者可以观察、丢弃、延迟、复制和注入其控制的部分 channel，典型范围是穿过网络的一处 cut。Envelope key 和 endpoint message key 对攻击者保密，未被观察的 channel 仍提供路径多样性。

## 9. Session 状态机

KCP 运行在 logical endpoint session 之间，作为端到端可靠传输机制。一个 shell 暴露给 core 的逻辑连接对应一个 endpoint session，也对应一个 KCP session。

Session 使用 full-close：任意一端发送 Close 都表示整个逻辑 session 进入关闭流程，两个方向停止接受新 Data。Close 作为有序 KCP message 可靠发送；主动关闭的一侧等 `wait_snd() == 0`，确认此前数据和 Close 均已被 ACK。接收端关闭 shell 资源，并暂时保留 KCP 状态以重复 ACK Close。关闭握手由一条可靠 Close 和 KCP ACK 构成。

Core 只表示整个 logical session 的关闭，不解释 TCP FIN 或其它应用协议的单方向结束。需要保留 half-close 的 Shell 在自己的 session 协议中发送方向性 EOF 控制消息；对端 Shell 收到后关闭本地连接的写方向，同时继续转发反向数据。两个方向都结束后，Shell 再提交 `SessionClose`。方向性 EOF 后的超时、reset 或不可恢复 IO 错误也可以直接触发 `SessionClose`。

不需要 half-close 的 Shell 可以把本地 EOF 直接映射成 `SessionClose`。因此 Core 的 full-close 状态机保持统一，具体代理协议决定是否以及如何表达单方向结束。Session 生命周期由 shell 和 core 共同推进：

```text
Idle
  --SessionOpen event / SessionCreated result--> Open
  --authenticated unknown conv--> PendingRemote

PendingRemote
  --first reassembled message is Open--> Open + OpenSession action
  --first message is Data/Close or timeout--> drop

Open
  --Data--> Open
  --local SessionClose event--> ClosingLocal
  --remote Close message--> ClosingRemote

ClosingLocal
  --remote Close message--> ACK, remain ClosingLocal
  --KCP send queue drained--> Closed
  --close deadline--> Closed

ClosingRemote
  --duplicate/retransmitted Close--> ACK again, remain ClosingRemote
  --close deadline--> Closed

Closed
  --tombstone expires--> Idle
  --late Open/Data/Close--> drop
```

状态机约束：

1. `SessionOpen` event 同步分配 `ConvId`，创建 `Open` 状态并把 Open 可靠写入 KCP，然后返回 `SessionCreated` result；同一次调用返回前 shell 已经知道该 `ConvId`。
2. KCP 可能先收到乱序的非零序号 segment，因此未知 `ConvId` 可以创建有超时上限的 `PendingRemote` 重组状态，但只有第一个完整 `SessionMessage` 是 Open 时才创建对 shell 可见的 session 并且只产生一次 `OpenSession` action。
3. Open 状态中的重复 Open、未知 session 的 Data/Close，以及 Closing/Closed 状态中的新 Data/Open 都被丢弃，不重复创建 shell handler。
4. 本地进入 `ClosingLocal` 后不再接受新的本地 Data。若此时 `wait_snd() == send_window`，Close 先保存在 session 状态中，待 ACK 释放槽位后再有序入队。双方同时主动关闭时，`ClosingLocal` 可以接收并 ACK 远端 Close，状态保持不变。因为本地 Close 最终是最后入队的有序 message，且完成判断前已经保证待入队 Close 被处理，`wait_snd() == 0` 可以作为本地主动关闭完成条件，不需要给 KCP 增加逐 segment ACK 回调。这个条件不能用于 `ClosingRemote`：接收侧可能没有待发送 segment，`wait_snd()` 会立即为零，但仍需保留接收状态来重新 ACK 对端重传的 Close。
5. 远端收到 Close 后立即向 shell 输出一次 `CloseSession` action，但 core 保留 `ClosingRemote` 的 KCP 接收和 ACK 状态，使丢失的 ACK 可以由对端重传 Close 再次触发。重复 Close 不重复通知 shell。
6. `CLOSE_DEADLINE_MS = 3 * max_rto`；在默认 `time_scale = 10` 下是 180 秒。到期后双方都停止为该 session 重传并释放完整 KCP 状态。随后保留轻量 Closed tombstone，直到 `SESSION_TOMBSTONE_MS` 到期；`SESSION_TOMBSTONE_MS` 至少覆盖最大 packet freshness window，防止关闭后的旧流量重新创建 session。

`SeqId` 同时标识发送 endpoint 的启动实例。EndpointCore 的协议状态使用以下主索引：

```text
sequence_filter: SeqId -> SequenceWindow
sessions:        ConvId -> KcpSession
tombstones:      ConvId -> expire_at
```

Relay 在 Envelope AEAD 认证后更新 `sequence_filter`；Endpoint 在 Envelope 和 endpoint message 两层 AEAD 都认证成功后才更新，因此 Endpoint 的 window 天然都对应真实 peer sequence，不需要额外标记。`newest_time` 提供超过 soft cap 时的淘汰依据；session 的创建、绑定和 KCP input 也发生在同一认证边界之后。持有 envelope key 但没有 message key 的节点不能创建或刷新 Endpoint 的 sequence window，也不能触发 peer 淘汰。

EndpointCore 允许多个 peer sequence 共存，soft cap 当前为 1，以后可以提高。只有 window 数量超过 cap 时，`time_diff(current_time, newest_time) > MAX_PAST_TICKS` 的 window 才可淘汰；Core 从最旧的可淘汰 window 开始删除，直到数量不再超过 cap，并以 `Reset` 关闭所有绑定到对应 `SeqId` 的 session。没有 window 可淘汰时暂时保持超额；相关过期时间参与 `next_deadline`，由 `poll` 执行清理。延迟到达的旧 packet 只能更新自己的 sequence，不能替换新 sequence 或破坏其 session；同一 time tick 内连续重启也不需要额外排序。

每个 session 保存 `peer_seq: Option<SeqId>`。远端发起的 session 在收到第一份 KCP packet 并创建 PendingRemote 时绑定；本地发起的 session 在第一份有效入站 KCP packet 到达时绑定，在此之前保持未绑定并继续重传 Open。绑定后只接受相同 `SeqId` 的 KCP packet。sequence window 被淘汰时一并删除其 KCP、PendingRemote 和相关临时状态；route graph 的长期健康状态保留。

KeepAlive 刷新对应的 sequence window，但不是保留 cap 内 sequence 的必要条件。只有存在超额 sequence 时，长期未刷新的 window 才会进入淘汰流程。

一个 `EndpointCore` 可以同时维护多个 endpoint session。每个 session 独立持有 KCP 状态和重传队列。收到已认证 endpoint message 后，core 先从 KCP segment header 读取 `conv`，再用该 `ConvId` 找到对应 KCP session。

一个 `Kcp` 实例对应一个 conv。EndpointCore 维护 `ConvId -> KcpSession` map，并在把 segment 交给 KCP 前完成 demux。session 是按 `ConvId` 查询的主状态，`peer_seq` 只记录归属；peer sequence 淘汰是低频操作，直接扫描 session map 清理匹配项，不维护反向索引。

`SessionMessage` 位于 KCP reassembly 之后，只表达这条连接的 open、data 和 close，不重复定义可靠传输序列语义。KeepAlive 和 RouteReply 属于 endpoint control plane，不占用任何 session 的 KCP 队列。

## 10. Channel 与 RoutePlan

每个节点启动时加载固定本地 channel table，也就是本节点可发送的 `ChannelId` 集合。`ChannelId` 只在所属节点的命名空间内有意义。真实 transport 目标和连接资源由 shell 管理。

一个 `ChannelId` 表示一条由 shell 定义的逻辑 route edge。物理连接到 `ChannelId` 的映射由 shell 决定：多条连接可以聚合在同一个 `ChannelId` 后面，由 shell 完成本地调度和 failover；也可以分别暴露为不同 `ChannelId`，作为由 endpoint 独立学习和选择的 edge。Relay 按 RoutePlan 指定的 `ChannelId` 转发。

Core 的 `SendTransportPacket` 只交给对应 Channel 的逻辑 Sender，网络收到的完整 packet 只由逻辑 Receiver 作为 `TransportPacketReceived` 交给 Core。Receiver 所在节点是否主动建立底层连接与这个数据方向无关。

Endpoint 的 `route.graph` 可以引用路径上其它节点的本地 `ChannelId`，因为发送 endpoint 负责生成该方向的完整 plan。

发送方 endpoint 为 outbound envelope 生成单向 route plan。一个完整 plan 是一串“各节点本地要使用的 ChannelId”：

```text
FullPlan = [Entry_ch5, RelayA_ch3, RelayB_ch7, RelayC_ch2]
```

当前节点发送 envelope 前先消费自己的 `ChannelId`，并把剩余 plan 写入 envelope：

```text
Entry consumes Entry_ch5
Envelope.route_plan = [RelayA_ch3, RelayB_ch7, RelayC_ch2]
SendTransportPacket { channel_id: Entry_ch5, bytes }

RelayA consumes RelayA_ch3
Envelope.route_plan = [RelayB_ch7, RelayC_ch2]
SendTransportPacket { channel_id: RelayA_ch3, bytes }

...

RelayC consumes RelayC_ch2
Envelope.route_plan = []
SendTransportPacket { channel_id: RelayC_ch2, bytes }
```

接收 endpoint 收到 `route_plan` 为空的 envelope 后，把 payload 交给 endpoint session。

Relay 的正常转发规则只有一种：`route_plan` 非空时，消费第一项作为本地 outbound `ChannelId`，把剩余 plan 写回 envelope 并发送。

Relay 完成 envelope 认证和结构校验、消费本地 `ChannelId` 后，必须在生成转发 action 前检查下一跳最小 packet 长度：

```text
forward_min_len =
    TRANSPORT_PACKET_OVERHEAD
  + ENVELOPE_FIXED_OVERHEAD
  + payload.len
  + remaining_route_hops * CHANNEL_ID_SIZE
  + MIN_PADDING
```

只有 `forward_min_len <= min(local_min_mtu, MAX_TRANSPORT_PACKET_SIZE)` 才能转发，否则这个 packet 不可能在本地 MTU 与协议包长契约内重新封装，必须作为非法 packet 丢弃并更新 metrics。这里检查的是消费一跳后的转发最小长度，不要求收到的原始 packet 长度小于 `local_min_mtu`；上游 MTU、旧 route plan 和旧 padding 都可能与本地不同。

已知 channel 的真实发送失败由 shell 把失败 action 交还给产生它的 core。Endpoint 立即标记失败并重新选路；relay 只记录本地失败并丢弃当前 packet，最终由发送 endpoint 的 KCP 重传恢复。

## 11. 调度与 MTU

Endpoint 在完整 DAG 上执行预计最早抵达调度，同时利用并行 edge、共享下游容量和低延迟路径。完整 `RoutePlan` 按 packet 生成，持久运行时状态只与 graph edge 数线性相关。

### 11.1 Edge 虚拟服务时间

Endpoint 为每条 edge 维护：

```text
EdgeRuntime
  virtual_free_at
  loss_ewma
  cooldown_until
  retry_delay_ms
  last_sampled
  last_observation_at
```

`virtual_free_at` 是这条 edge 已被本 EndpointCore 的所有 session 预约到的虚拟完成时间。它使用内部固定点时间，不受 KCP `time_scale` 影响，也不直接延迟 `SendTransportPacket` action；它只让后续 packet 的选路计入先前分配给各 edge 的工作量。

`loss_ewma` 是 `[0, 1]` 之间的连续失败分数，用来平滑 channel 介于完全健康和完全不可用之间的状态。它不是对真实 packet loss rate 的无偏测量：本地 send failure 是强失败信号，RouteReply timeout 还可能来自反向 route，所以只是弱失败信号。

EndpointCore 构造时乐观地令所有 edge 的 `loss_ewma = 0`、`virtual_free_at = 0`、`retry_delay_ms = 50`，`cooldown_until` 为空，`last_sampled` 和 `last_observation_at` 为从未发生。相同预计抵达时间使用 graph 中较早的 edge 作为确定性 tie-break。

调度使用与实际随机 padding 无关的保守长度：

```text
scheduled_bytes =
    endpoint_payload_len
  + TRANSPORT_PACKET_OVERHEAD
  + ENVELOPE_FIXED_OVERHEAD
  + padding_reserve
  + (longest_route_hops - 1) * CHANNEL_ID_SIZE
```

Core 在构造时把零值 hint 替换为默认值，调度期只使用归一化后的固定先验：

```text
edge.capacity_kbps = if capacity_hint_kbps == 0 {
    DEFAULT_ROUTE_CAPACITY_KBPS
} else {
    capacity_hint_kbps
}

edge.latency = if latency_hint_ms == 0 {
    DEFAULT_ROUTE_LATENCY_MS
} else {
    latency_hint_ms
}

edge.service_penalty = 1 + 15 * edge.loss_ewma * edge.loss_ewma
edge.effective_capacity_kbps = edge.capacity_kbps / edge.service_penalty
serialization_time_ms = scheduled_bytes * 8 / edge.effective_capacity_kbps
```

这些运算在实现中使用固定点整数。因为 `Kbps` 中的千和每秒到毫秒的换算恰好抵消，`scheduled_bytes * 8 / effective_capacity_kbps` 直接得到毫秒。`effective_capacity_kbps` 只是 route scheduler 的运行时服务速率，不回写 capacity hint，也不声称学习到了物理带宽。平方项让单次弱 timeout 的影响很小，只有连续失败才明显降低长期分流份额；最高 16 倍的服务惩罚使 edge 仍保留 `1/16` 的名义份额和恢复机会。指定 edge 的 feedback sample 不受这个份额限制。只有本地已确认 send failure 的 first edge 会在 cooldown 到期前临时不参与计算。

对每个待发送的 endpoint packet，Core 按 graph 的拓扑顺序计算从 source 到每个节点的预计最早抵达时间：

```text
arrival[source] = now

for node in topological_order:
  for edge in node.edges:
    start = max(arrival[node], edge.virtual_free_at)
    candidate =
        start
      + serialization_time_ms(packet, edge)
      + edge.latency
    relax arrival[edge.next] with candidate
```

Core 回溯 destination 的 predecessor 得到完整 `RoutePlan`，随后沿 plan 预约每条 edge：

```text
cursor = now
for edge in selected_plan:
  start = max(cursor, edge.virtual_free_at)
  edge.virtual_free_at = start + serialization_time_ms(packet, edge)
  cursor = edge.virtual_free_at + edge.latency
```

一次计算和回溯的时间复杂度是 `O(nodes + edges)`。平行 Channel 各自拥有独立的 first-edge 服务时间；如果它们汇入同一个较慢的 downstream edge，所有相关路径共享那一份 `virtual_free_at`，因此 Core 会自动限制这组路径的总份额。任意更复杂的分叉、汇合和共享后缀都使用同一规则。

预计最早抵达调度会先使用固定延迟较低的路径；随着其虚拟队列增长，其它路径在预计抵达时间更小时自然参与分流。`loss_ewma` 连续降低近期失败 edge 的长期分流份额，而不把它切换成 up/down 两态。选路由归一化 hint、`loss_ewma` 和虚拟队列统一确定。每份 KCP datagram 只有一个成功离开本节点的 wire 副本。

Shell 明确报告 first-edge 本地发送失败时，Endpoint 立即强更新该 edge 的 `loss_ewma` 并进入 cooldown，再为同一个 KCP packet 重新生成 envelope 和完整 RoutePlan。本地恢复退避使用固定的 `50ms -> 100ms -> 200ms`，随后保持 200ms 上限。失败发生在 packet 离开本节点之前，下一条可用 edge 可以在同一次 shell action 执行循环之后继续承载该 KCP packet。已经形成的虚拟预约是短期预测状态，随时间推进自然失效。Relay 的本地发送失败仍然结束当前 packet，由发送 endpoint 的 KCP 恢复。

Route scheduler 只选择 route 和分配名义容量，不控制 session 的发送窗口。每个 endpoint session 各自拥有一个 KCP；该 session 的所有 route 被 KCP 视为一个 aggregate path，原生 congestion control 根据混合后的 ACK、loss 和 in-flight data 控制该 session 的发送量，`send_window` 是其上限，`SessionWriteBlocked` 把压力继续传给 Shell。不同 session 的 KCP congestion state 相互独立。这不是 per-edge congestion control：单条路径拥塞可能收缩整个 session 的窗口，但状态和 KCP 算法保持单一。

### 11.2 Route Reply 采样

Route feedback 的采样单元是完整 endpoint packet，语义独立于 KCP header、ACK 和 segment number。`KcpPacket` 和 `KeepAlive` 可以发起 feedback chain，`RouteReply` 可以在回复对端的同时继续这条 chain。每份 `reply_depth > 0` 的 packet 都是发送端的一个独立样本，并关联发送时使用的完整 `RoutePlan`。

```text
RouteFeedbackState
  next_request_at
  outstanding: Map<PacketId, FeedbackSample>

FeedbackSample
    packet_id: (seq_id, seq_no)
    route_plan
    sent_at
    deadline
```

Core 启动时 `next_request_at = 0`，第一份可以携带非零 depth 的实际 outbound packet 立即成为样本。之后每经过一个 `route.feedback_interval_ms`，下一份候选 packet 可以成为新样本，无需等待先前 RouteReply。Outstanding 数量的上界由发送速率和过期时间自然确定：

```text
max_outstanding = ceil(route.feedback_timeout_ms / route.feedback_interval_ms) + 1
```

这个窗口允许大量 route 在长 RTT 网络中并行收敛，同时让每个 endpoint 产生的请求速率和 outstanding 状态量由配置直接限定。收到对端的非零 depth 一定产生一份 reply，但是否让该 reply 继续请求反向 reply，仍由本 endpoint 的 interval 预算和深度上限决定。

采样流程固定为：

1. 当 `elapsed_ms >= next_request_at` 时，下一份待发送的 `KcpPacket` 或 `KeepAlive` 用 `reply_depth = 1` 发起 chain。如果待发送的是对非零 depth 的 `RouteReply`，它在收到的 depth 小于 3 时用加一后的 depth 继续 chain；收到 3 时只能回复 0。
2. Core 用该 outbound envelope 自己的 `(seq_id, seq_no)` 作为 PacketId，记录本次使用的完整 `RoutePlan` 与 `deadline = sent_at + route.feedback_timeout_ms`。
3. 对端认证并接受非零 depth 的 endpoint packet 后，确定性地立即发送一份 `RouteReply { received_packet_id }`。对端当前没有自己的采样预算时，这份 reply 的 depth 为 0。
4. 发起方收到匹配的 RouteReply 后，确认该完整 forward route 至少成功到达一次，把 plan 上所有 edge 作为成功样本并删除对应 entry。如果这份 RouteReply 自身的 depth 非零，同一次接收还按第 8.3 节产生下一份 reply。
5. 每次实际成功离开本节点的非零 depth 都把 `next_request_at` 推迟至少 `route.feedback_interval_ms`，再附加非负随机 jitter。Reply 提前到达也不能突破这个最小间隔。

普通 packet 和 depth 为 0 的 RouteReply 使用预计最早抵达 plan。每份本地 feedback sample 选择 cooldown 已结束且 `last_sampled` 最旧的可达 edge，并用状态为 `(node, selected_edge_used)` 的两层 DAG dynamic programming 构造预计最早抵达、且包含该 edge 的完整 route。这个受约束计算仍为 `O(nodes + edges)`。发送时更新 plan 上 edge 的 `last_sampled`。连续 sample 因而覆盖不同 edge，长 RTT 不会让调度器停留在同一条待回复 route。被选中的普通 KCP packet 或 RouteReply 可能走探索 route；它们的 timeout 语义保持一致。

成功匹配 RouteReply 是 forward plan 已到达对端的强成功证据。RouteReply 可能在反向 route 丢失，所以 deadline 到期只是对该 forward plan 的弱失败证据。两个方向分别以自己的 sample 独立更新 edge，不从一次 timeout 反推具体是 forward edge 还是 reverse edge 丢失。

`loss_ewma` 使用固定增益，内部以固定点整数实现：

```text
on_local_send_failure(loss) = loss + (1 - loss) / 2
on_feedback_timeout(loss)   = loss + (1 - loss) / 16
on_feedback_success(loss)   = loss * 3 / 4
```

明确本地失败最快提高分数，有反向歧义的 timeout 缓慢提高分数，受认证的成功 reply 逐步降低分数。这三个转移都保留中间状态，不产生隐式 up/down 阈值。

状态更新固定为：

1. Shell 明确报告任意 packet 的本地 send failure：强更新本地第一条 edge 的 `loss_ewma`，并启动第 11.1 节的短 cooldown。若它是 outstanding sample，删除对应 entry 并令下一份成功发送的候选 packet 立即成为 sample；KCP packet 重新封装后换 route 发送，KeepAlive 和 RouteReply 则结束。未离开本节点的 sample 不消耗反馈速率预算。
2. RouteReply 到达：成功更新对应完整 forward route 上的每条 edge。
3. Reply deadline 到期：弱失败更新对应完整 forward route 上的每条 edge，并删除 entry。
4. RouteReply body 中未匹配、重复或迟到的 PacketId 幂等忽略。

多个 sample 可以乱序完成。每次本地确定失败使用失败发生的 `elapsed_ms`，RouteReply 或 timeout 使用 sample 的 `sent_at`，作为这次结果的 observation time。结果只更新 `observation_time >= edge.last_observation_at` 的 edge，并同时推进 `last_observation_at`；因此旧 reply 或旧 timeout 不会覆盖该 edge 上已经发生的新失败或新反馈。被接受的 first-edge 成功样本同时结束 cooldown 并把 `retry_delay_ms` 复位为 50ms。

KeepAlive 与这个机制只有组合关系，没有自己的请求/响应语义：有业务流量时，按固定时间速率选择的普通 KCP packet 承担采样；没有业务流量但 KeepAlive 启用时，下一份 KeepAlive 承担采样；恰好正在回复对端时，本端的采样可以搭载在 RouteReply 上。活跃 KCP session 连续 RTO 或长时间无进展时，下一份重传 packet 在最小 feedback interval 允许的时刻优先成为样本；`loss_ewma` 仍只由本地发送结果、RouteReply 和 feedback timeout 更新。Shell 设置的 `feedback_timeout_ms` 覆盖部署中有效 forward route、reverse reply route 及其正常排队时间。

KCP MTU 在 EndpointCore 构造时按最坏 route 固定计算：

```text
remaining_hops = longest_route_hops - 1
packet_overhead =
    TRANSPORT_PACKET_OVERHEAD  // 32: envelope nonce + tag
  + ENVELOPE_FIXED_OVERHEAD    // 16: time_and_route + seq_id + seq_no
  + ENDPOINT_MESSAGE_OVERHEAD  // 17: endpoint-message tag + flags
  + padding_reserve            // shell 提供，示例 128
  + remaining_hops * CHANNEL_ID_SIZE

kcp_mtu = min(route.min_mtu, MAX_TRANSPORT_PACKET_SIZE) - packet_overhead
mss     = kcp_mtu - KCP_OVERHEAD   // KCP_OVERHEAD = 28
```

`padding_reserve` 预留的是“即使 KCP packet 满载也仍可随机变化的长度空间”。它不等于每个包必须使用的 padding。实际发送时：

```text
available =
    padding_reserve
  + unused_kcp_space
  + unused_route_space

padding_len = random(1 ..= min(255, available))
```

较短 KCP packet 或较短实际 route 会留下额外空间，因此首包和短包经常可以把 padding 上限扩到 255。满载 packet 仍至少有 `padding_reserve` 种长度可选。

Relay 每一跳都会重新封装，因此只需要知道本节点 outbound channel 的 MTU。第 10 节的 `forward_min_len` 检查保证转发 packet 至少能容纳固定开销、剩余 route、payload 和一个 padding byte；随后 relay 才在剩余空间内选择实际 padding。

最大接收包长是资源保护上限：

```text
MAX_TRANSPORT_PACKET_SIZE = 64 KiB
```

该上限作用于一份尚未认证的外层 `TransportPacket`，也封顶 Core 内部用于组包的有效 MTU。`route.min_mtu` 和 `local_min_mtu` 可以更大，但超出的物理容量不会扩大 RayNet wire packet。超过该上限的 inbound transport packet，在认证和分配大缓冲前直接丢弃。

`MAX_SESSION_DATA_SIZE` 作用于 KCP reassembly 之后的一份应用数据消息。一份最大 SessionMessage 可以分成多个 KCP segment 和多份 TransportPacket；一份 TransportPacket 也可以聚合多个较小的 KCP segment。两个 64 KiB 上限分别约束重组后的逻辑消息和重组前的单个外层 packet，互不推导。

KCP 始终使用本地单调 `elapsed_ms`。Endpoint session driver 在每次 KCP input 时传入本次 event 的 `elapsed_ms`，使 ACK RTT 计算不依赖上一次 poll/send 留下的旧时间；input 更新时间采样但不因此触发 timer flush。`time_scale` 缩放 KCP 的 update interval、RTO 下限等时间参数，而不是缩放时钟。KCP session 构造时一次性接收 MTU、窗口、`nodelay`、fast-resend threshold、是否启用 congestion control 和 `time_scale`；这些参数在 session 生命周期内不可改变。RayNet 固定使用 `nodelay = true`、关闭原生 KCP congestion control，并用 `NonZeroU32::new(KcpConfig.fast_resend)` 得到可选 threshold；参考 shell 默认使用 `32`。

KCP 构造时把 `time_scale` 提升为 `u32`，并直接计算以下毫秒参数：

```text
interval          = 2      * time_scale
nodelay_min_rto   = 3      * time_scale
normal_min_rto    = 10     * time_scale
initial_rto       = 20     * time_scale
max_rto           = 6000   * time_scale
probe_initial     = 700    * time_scale
probe_max         = 12000  * time_scale
```

Core API 默认使用 `time_scale = 10`。KCP 输出是不可失败的内存 packet 队列；channel 发送结果属于 core 与 shell 的边界。

## 12. 随机性

Core 使用单个共享 DRBG：

```text
random_seed -[BLAKE3]-> PRK -[keyed XOF]-> continuous pseudorandom bytes
```

实现上对应一个 BLAKE3 keyed `OutputReader`。API 只提供：

```text
fill(&mut [u8])
u64()
```

Envelope nonce、padding、SeqId 和 ConvId 起点都从这条流顺序消费。SeqId 取随机 `u64` 的低 48 bits。共享 DRBG 不需要额外 domain：不同用途读取互不重叠的伪随机字节，等价于通用系统 CSPRNG 的使用模型。

约束：

1. 每个 Core 实例必须获得新的均匀随机 `random_seed`。
2. 随机状态不得 Clone、Seek、复位或导出。
3. 同一输出不得使用两次。
4. 128-bit envelope nonce 是伪随机值；碰撞概率受同一 envelope key 的消息预算约束。
5. Debug 不得泄露 PRK 或随机状态。

调用顺序变化会改变后续随机值。RayNet 不承诺内部随机序列兼容，因此这不是协议问题。

## 13. Time 与 Replay

```text
GlobalTime = boot_time_ms + elapsed_ms

handle_event(elapsed_ms, ...)
  process one external event
  may emit actions

poll(elapsed_ms, ...)
  advance timers, retransmission, keepalive, route feedback, queue aging
  may emit actions

next_deadline(elapsed_ms) -> u64
  earliest time core wants poll(elapsed_ms) to be called
  u64::MAX means no deadline
```

Core 不 sleep，不 spawn，不设置系统 timer。Shell 把 `next_deadline` 映射到真实 timer 或测试虚拟时间。

Core 重启会创建全新的协议实例和 random seed，新的 `elapsed_ms` 从零开始。Shell 在重启瞬间重新采样当前 `GlobalTime` 作为新的 `boot_time_ms`，因此 `GlobalTime = boot_time_ms + elapsed_ms` 在重启前后保持连续，而单调调度时间属于各自独立的 core 生命周期。单个 EndpointCore 实例的生命周期不超过一年。

RayNet 使用两个时间域：

1. `elapsed_ms` 来自本机单调时钟，用于 KCP timer、调度、衰减和 deadline。KCP wire `ts` 属于这个时间域，只由 ACK 回显给原发送端计算 RTT。
2. `GlobalTime` 由启动时的全局时间基准和 `elapsed_ms` 派生；Envelope `time` 是发送端 GlobalTime 的低精度表示，用于跨节点 packet freshness。

接收和同步认证发生在同一次 `handle_event` 中，共用当次 `elapsed_ms`，不构成新的时间域。收到得更晚不代表生成得更新，因此 `SequenceWindow.newest_time` 只依据认证后的 Envelope `time`，不用接收时刻或 KCP `ts`。

Replay protection 使用 envelope 中三个固定宽度字段：

```text
time: 26-bit field in time_and_route
seq_id: u48
seq_no: u48

TS_SHIFT = 10
MAX_PAST_TICKS = 645
MAX_FUTURE_TICKS = 59
SEQUENCE_WINDOW_BITS = 1 << 20
```

`time` 保存 `(GlobalTime_ms >> TS_SHIFT) & ((1 << 26) - 1)`。接收方计算 26-bit modular difference `age_ticks = current_time - packet.time`，接受 `-MAX_FUTURE_TICKS <= age_ticks <= MAX_PAST_TICKS` 的 packet。

每个接收 core 维护 `seq_id -> SequenceWindow`：

```text
SequenceWindow
  max_seq_no: u64  // 保存 u48 wire value
  newest_time: u32 // 保存 u26 wire value
  bitmap: 2^20 bits = 128 KiB
```

Bitmap 表示 `[max_seq_no.saturating_sub(2^20 - 1), max_seq_no]` 范围内已经收到的序号，并作为循环位图使用：

```text
bit_index = seq_no & (SEQUENCE_WINDOW_BITS - 1)
```

窗口内重复 `seq_no` 被拒绝，窗口前的序号被视为过旧。收到更大的 `seq_no` 时，若前进距离小于窗口大小，只清除新进入窗口的循环区间；若前进距离大于等于窗口大小，直接清空整个位图。

`newest_time` 是该 `seq_id` 已通过对应认证边界和 replay check 的 packet 中按 `time_diff` 判断最新的 `time`。Relay 没有 endpoint session，在插入新的 `seq_id` 时可以直接清理过期 window，不需要独立 timer。Endpoint 在 window 数量超过 soft cap 时才把可淘汰 window 的过期时间纳入 `next_deadline`，并在 `poll` 中删除 window、清理绑定到该 `SeqId` 的 session；数量不超过 cap 时保留 window。

Relay 的接收顺序是：检查长度，完成 Envelope AEAD，检查 freshness/replay 并更新 sequence window，然后转发。Endpoint 先完成 Envelope 和 endpoint message 两层 AEAD，再检查 freshness/replay、更新 sequence window 并处理 endpoint message。对应失败都安全丢弃并更新 metrics。

RayNet 的 replay 状态作用域是单个接收 core 的进程生命周期。全局时间窗口限制跨重启旧包的有效期；重启后仍处于 freshness 范围内的已认证 packet 可能再次被接受。该范围符合当前攻击模型和内存状态模型。

Relay 无法读取 endpoint identity，因此 replay map 把 `seq_id` 当作所有经过该 RelayCore 的 packet sequence namespace。

## 14. Metrics

```text
CoreMetrics
  packets_received
  packets_sent
  packets_dropped
  authentication_failures
  freshness_drops
  replay_drops
  malformed_packets
  send_failures
  active_sessions
  active_sequences
  latest_seq_time
```

`core.metrics()` 返回只读快照。packet、failure 和 drop 字段是累计 counter；`active_sessions`、`active_sequences` 和 `latest_seq_time` 是当前 gauge。Shell 对 counter 计算采样差值，直接读取 gauge，再输出日志、Prometheus 或 tracing。

`latest_seq_time` 用于 Shell 观察最近一次已认证入站流量，不复用 window 中截断的 `newest_time`。Core 在接收 packet 时令 `current_tick = (GlobalTime_ms >> TS_SHIFT) as u32`，先计算已经通过 freshness 检查的 `age_ticks`，再以 `packet_tick = current_tick.wrapping_sub(age_ticks as u32)` 恢复 packet 所属的完整 tick；只有它比当前 gauge 更新时才替换。这样跨过 26-bit 边界时仍选择离当前时间最近的正确值，而不是直接填充当前时间的高位。`active_sequences == 0` 时忽略该值；它不参与 sequence 淘汰或 session 生命周期。

非法 packet 使用累计 metrics 表达，shell 定期采样差值并控制日志速率。Runtime API 返回状态机结果，Shell 契约违约使用 fail-fast 语义。

## 15. 输入安全与 Panic

输入边界：

1. `TransportPacketReceived.bytes` 是网络进入 core 的不可信输入。
2. 未通过 Envelope AEAD 的 transport bytes 只能触发廉价解析、认证尝试、丢弃或更新 metrics，不能创建协议状态。
3. Relay 在 Envelope 认证后创建 replay window；Endpoint 只有在 endpoint message AEAD 认证成功后才创建 replay window、session、route feedback 或 KCP 状态。
4. 网络包里的可变长字段先验证长度再分配。

Panic 原则：

1. Panic 用来暴露实现 bug 和保留现场，不是输入校验机制。
2. 可信 API 输入进入 core 后按调用契约已经成立来实现。Envelope 层的认证、长度、freshness 和 replay 错误安全丢弃；endpoint message AEAD 认证成功后的畸形 Message 或 KCP 表示实现 bug，可以 fail-fast。
3. Core 内部可以用无文本断言表达已经由类型、Shell 契约或网络输入边界检查保证的不变量。
4. Core 不主动调用 `process::exit`；但在 `panic = "abort"` 构建配置下，panic 会终止进程，这是部署策略的一部分。
5. 直接构造和调用 core 的使用者负责满足全部可信 API 契约。

Core 不实现复杂全局资源配额。设计假设攻击者不知道 PSK 或私钥；资源防护放在认证前检查接收缓冲区大小、Envelope 认证失败不建状态、解析可变长字段前检查边界和自然 backpressure 上。

## 16. 连接模型

```text
Entry:
  ingress / proxy handling <-> shell session map <-> EndpointCore
  EndpointCore -> RoutePlan -> shell channel connection -> EndpointCore

Exit:
  EndpointCore <-> shell session map <-> egress / proxy handling
  EndpointCore -> RoutePlan -> shell channel connection -> EndpointCore

Relay:
  shell channel connection -> RelayCore -> shell channel connection
```

同一个 relay mesh 可以同时服务多组 entry 和 exit。Relay 不理解 entry/exit 配对关系；它只验证 envelope、消费 route plan 并转发。一个 entry 的 `RouteConfig` 描述本 endpoint 到明确 exit 的发送方向；回程由该 exit 的独立 `RouteConfig` 指向对应 entry。

Core session 是可靠、有序的 opaque message 序列。它足以支持：

1. SOCKS5 / HTTP CONNECT 等应用代理。
2. 简单的点对点 L3 TUN VPN。
3. 简单的点对点 L2 TAP VPN。

默认 UDP channel 本身就是不可靠 datagram transport；丢包、重复和重排序由端到端 KCP session 处理。这与“向应用暴露不可靠 datagram session”是两件事。Core 向 shell 暴露可靠、有序的 session；host/port、network type 和具体 VPN 语义属于 shell。

## 17. 未来方向

1. `no_std + alloc`，让 core 能进入更受限的嵌入环境。
2. 更窄、更稳定的 C ABI，用于 C、Python、BEAM VM 等 shell。
3. 更多 shell transport channel，包括 mailbox/file、HTTP polling、GitHub upload/download。
4. 更成熟的 edge-state 和 route generation 策略。
5. 面向不同网络形态的可靠传输参数。
6. 多目标通信：评估一个进程内运行多个指向不同 destination 的 endpoint core。

核心边界保持为：core 是同步状态机，shell 负责运行时、ingress/egress、代理协议和 channel 连接，relay 处理 envelope 和 route plan，endpoint 处理 endpoint message 和自己发送方向的 route-plan 调度。
