# RayNet 设计文档

## 1. 当前目标

RayNet 是一个由 Rust core 和 runtime shell 共同组成的 overlay network。Core 是同步状态机，负责协议、加密、KCP、stream mux、relay forwarding 和 channel 调度。Shell 负责真实 IO、timer、配置、日志、ingress 和 exit connector。

当前设计聚焦三件事：

1. Entry endpoint 接收本地应用连接，选择穿越网络的 channel plan。
2. Relay 按 envelope 转发，不理解 endpoint payload。
3. Exit endpoint 连接被代理的真实服务，并把返回流量送回 entry。

传输 channel 可以是 UDP、TCP、HTTP、DNS、mailbox/file、GitHub upload/download 等。Channel 可以是有向的；`A -> B` 存在不代表 `B -> A` 存在。Relay 和 endpoint 的 channel 集合在进程生命周期内固定；增删或重编号 channel 需要重启相关节点，并同步 entry 的拓扑配置。

设计文档是架构决策的主要依据。重要设计变化应先写进本文档，再落到代码里。

## 2. 分层

```text
Application Edge
  ingress connector
  exit connector
  management/config

Runtime Shell
  Tokio shell
  test shell
  future C shell
  future Python shell
  future BEAM shell
  future shells

RayNet Core
  sync state machine
  envelope framing
  hop authentication
  endpoint encryption
  KCP session
  stream mux
  channel-plan forwarding
  channel health and scheduling
```

Core 不直接执行 IO，不读取系统时间，不 spawn task，不 sleep，不访问 socket、文件、环境变量或网络 API。Shell 把外部事件转成 `CoreEvent`，core 推进状态并输出 `CoreAction`，shell 执行动作。

## 3. 语言策略

Core 必须用 Rust 实现。当前最重要的 shell 也是 Rust shell，主要用于本地开发、Tokio IO、测试和基线实现。

后续可以接入 C、Python、BEAM VM 或其它 runtime shell。跨语言 shell 不重新实现协议逻辑，只调用 Rust core，并负责各自 runtime 下的 IO、timer、process lifecycle、配置和日志。跨语言边界需要单独设计窄接口；在当前阶段，Rust-native API 优先。

## 4. 节点角色

```text
Entry Endpoint
  local ingress connection
  topology view
  channel plan generation
  plan health learning
  endpoint session and KCP

Relay
  envelope authentication
  channel-plan consumption
  return-trace recording
  local channel forwarding

Exit Endpoint
  exit connection
  endpoint session and KCP
  return forwarding
```

Entry endpoint 可以持有全网拓扑和调度状态。Relay 和 exit endpoint 不需要全网拓扑；它们只需要自己的本地 channel table、简单目的节点转发表，以及当前 envelope 携带的信息。

## 5. Rust Core API

Rust 版本先使用 Rust-native API。

```text
EndpointCore::new(config) -> Result<EndpointCore, ConfigError>
RelayCore::new(config) -> Result<RelayCore, ConfigError>

core.handle_event(now, event, action_sink) -> Result<(), CoreError>
core.poll(now, action_sink) -> Result<(), CoreError>
core.next_deadline(now) -> Option<deadline>
```

`now` 是 shell 传入的虚拟单调时间。Core 不主动查询时间。Shell 在收到外部事件时调用 `handle_event`，在 `next_deadline` 到期时调用 `poll`。`action_sink` 是 core 写入输出动作的临时 sink，Rust 实现可以直接使用 `&mut Vec<CoreAction>`。

## 6. 标识

```text
NodeId
  节点身份，标识 entry、relay、exit

ChannelId
  某个节点本地的 transport channel 编号
  在 channel plan 和 return trace 中按当前节点的本地语义解释

EndpointId
  endpoint session 双方身份

StreamId
  endpoint session 内的协议 stream 编号
  由 core 分配，进入 EndpointFrame

LocalConnectionId
  endpoint shell 本地连接句柄
  只存在于 entry/exit endpoint，不进入 wire protocol

Target
  exit endpoint 要连接的目标描述
```

`LocalConnectionId` 只用于 endpoint。它对应 entry 侧 ingress connection 或 exit 侧真实服务连接。Relay 不使用 `LocalConnectionId`。

`StreamId` 是协议里的 logical stream id。Entry 和 exit 用它在 endpoint session 内对应同一条逻辑流。Shell 可以把 `StreamId` 当 opaque token 回传给 core，但不生成或解释它。

## 7. CoreEvent

```text
CoreEvent
  TransportPacketReceived {
    channel_id,
    bytes
  }
  TransportChannelUpdated {
    channel_id,
    state,
    metrics
  }
  IngressConnectionOpened {
    local_connection_id,
    target,
    metadata
  }
  ExitConnectionOpened {
    stream_id,
    local_connection_id
  }
  ExitConnectionOpenFailed {
    stream_id,
    reason
  }
  LocalConnectionBytes {
    local_connection_id,
    bytes
  }
  LocalConnectionClosed {
    local_connection_id,
    reason
  }
  ConfigUpdated(config_delta)
```

Connection 相关事件只用于 endpoint core。Relay core 只处理 transport、channel、config 和 timer 推进。

## 8. CoreAction

```text
CoreAction
  SendTransportPacket {
    channel_id,
    bytes
  }
  OpenExitConnection {
    stream_id,
    target,
    metadata
  }
  WriteLocalConnection {
    local_connection_id,
    bytes
  }
  CloseLocalConnection {
    local_connection_id,
    reason
  }
  CloseRemoteStream {
    stream_id,
    reason
  }
  EmitMetric(metric)
  EmitLog(level, event)
```

Shell 执行 `SendTransportPacket` 时必须使用 core 指定的 `channel_id`。发送失败、队列压力或 channel 断开通过后续 `TransportChannelUpdated` 反馈给 core。

## 9. Wire Protocol

```text
TransportPacket
  shell 收发的原始字节

Envelope
  relay 可读的节点间转发单元

EndpointPayload
  endpoint 间端到端加密和认证的数据

EndpointFrame
  KCP reassembly 后的 stream mux frame
```

Envelope 包含：

```text
Envelope
  magic/version/profile
  packet_type
  source_node_id
  destination_node_id
  channel_plan
  return_trace
  nonce
  auth_tag
  payload
```

`packet_type` 当前为：

```text
PacketType
  Data
  Control
```

Envelope payload 通常承载某个 endpoint session 的 KCP segment。KCP 负责可靠传输、序列号、重传和乱序处理。KCP reassembly 后，EndpointCore 再解析 `EndpointFrame`：

```text
EndpointFrame
  stream_id
  frame_type
  length
  payload

FrameType
  OpenStream
  StreamBytes
  CloseStream
  ResetStream
  KeepAlive
```

Hop-level auth 防止外部伪造或篡改 envelope。当前威胁模型信任已认证 relay，不把 relay compromise 作为主要防御目标。Endpoint-level encryption/auth 保护 endpoint payload，relay 不持有 endpoint payload 的解密能力。

数据格式使用固定 header、明确长度字段和 payload。Packet/envelope 有最大接收长度；可变长字段先验证长度再分配；整数、长度、时间和索引字段定义字节序、单位、溢出行为和非法值处理。

## 10. KCP 与 Stream

KCP 运行在 logical endpoint session 之间，作为端到端可靠传输机制，不绑定单个 link 或 channel。Relay 只转发 envelope，不理解 KCP segment、stream id、exit target 或 endpoint payload。

Channel plan 决定某个 envelope 怎么穿过 relay 网络；KCP 决定 endpoint session 内如何可靠传输。调度器可以把同一个 endpoint session 的不同 KCP segment 放进不同 envelope，经由不同 channel plan 发出。KCP 的重传、ACK、乱序处理和拥塞相关状态属于 endpoint core，不属于 relay。

`EndpointFrame` 位于 KCP reassembly 之后，只做 stream mux，不重复定义可靠传输序列语义。

## 11. Channel 配置

每个节点启动时加载固定 channel table：

```text
ChannelConfig
  channel_id
  peer_node_id
  channel_kind
  mtu
```

`ChannelId` 只在所属节点的命名空间内有意义。Entry 的拓扑配置可以引用其它节点的本地 `ChannelId`，因为 entry 是当前系统的 plan controller。Relay 和 exit 不需要理解其它节点的 `ChannelId`。

运行期只更新 channel 状态：

```text
TransportChannelUpdated
  channel_id
  state: Up | Down | Degraded
  metrics:
    queue_pressure
    send_error
```

## 12. 正向 ChannelPlan

Entry endpoint 为正向包生成 channel plan。一个完整 plan 是一串“各节点本地要使用的 ChannelId”：

```text
FullPlan = [Entry_ch5, RelayA_ch3, RelayB_ch7, RelayC_ch2]
```

当前节点发送 envelope 前先消费自己的 `ChannelId`，并把剩余 plan 写入 envelope：

```text
Entry consumes Entry_ch5
Envelope.channel_plan = [RelayA_ch3, RelayB_ch7, RelayC_ch2]
SendTransportPacket { channel_id: Entry_ch5, bytes }

RelayA consumes RelayA_ch3
Envelope.channel_plan = [RelayB_ch7, RelayC_ch2]
SendTransportPacket { channel_id: RelayA_ch3, bytes }

RelayB consumes RelayB_ch7
Envelope.channel_plan = [RelayC_ch2]
SendTransportPacket { channel_id: RelayB_ch7, bytes }

RelayC consumes RelayC_ch2
Envelope.channel_plan = []
SendTransportPacket { channel_id: RelayC_ch2, bytes }
```

Exit endpoint 收到 `destination_node_id == self` 且 `channel_plan` 为空的 envelope 后，把 payload 交给 `EndpointCore`。

如果 relay 发现 plan 中指定的本地 channel 不可用，它不尝试全局重路由。它丢弃该 packet 或发送 control failure；entry 通过 failure、timeout 和 session metric 重新选择 plan。这样实现保持简单，自适应逻辑集中在 entry。

## 13. 回包 ReturnTrace

Exit 到 entry 的回包不要求 exit 知道全网拓扑。Exit 和 relay 只使用本地目的节点转发表：

```text
DestinationForwardingTable
  destination_node_id -> candidate local ChannelId list
```

回包 envelope 携带 return trace：

```text
ReturnTrace
  ordered TraceEntry list

TraceEntry
  node_id
  channel_id
  optional metrics snapshot
```

Exit 发送回包时，为 `destination_node_id = entry` 选择本地 outbound channel，并写入第一条 trace。每个 relay 转发回包时按目的节点选择本地 channel，追加 trace，再转发。Entry 收到回包后用 trace 学习实际返回路径。

这个模型支持有向 channel。正向 plan 和返回 trace 可以完全不同，只要求 endpoint session 在端到端层面具备双向流量能力。

## 14. 调度与自适应

当前调度目标是简单可靠地绕开坏节点和坏 channel。

Entry endpoint 维护：

```text
PlanScore
  full channel plan
  delivery success
  retransmit rate
  goodput
  composite latency
  stall count

ReturnTraceScore
  observed return trace
  delivery success
  composite latency
  channel failures
  queue pressure

NodeHealth
  alive/degraded/down
  recent failures
```

Entry 根据配置拓扑生成候选 plan，避开已知 down/degraded 的节点和 channel。KCP 重传、control failure、send error、timeout、return trace 和 session 表现都会更新 score。调度算法可以很简单：优先使用健康 plan；失败后降权；连续失败后避开相关节点/channel；健康恢复后再逐步试探。

Relay 和 exit 只做本地选择：从目的节点转发表中挑一个健康 channel。它们不维护全网 score，也不把全网拓扑暴露给服务点。

MTU 属于 channel 配置。Entry 生成 plan 时使用保守全局 MTU 或 plan 中 channel 的最小 MTU，避免 envelope 超出任一 hop 的承载能力。

## 15. Time 与 Poll

```text
handle_event(now, ...)
  process one external event
  may emit actions

poll(now, ...)
  advance timers, retransmission, probes, queue aging
  may emit actions

next_deadline(now)
  earliest time core wants poll(now) to be called
```

Core 不 sleep，不 spawn，不设置系统 timer。Shell 可以把 `next_deadline` 映射到 Tokio timer、测试虚拟时间或离线批处理节奏。

## 16. Panic 与安全

输入边界：

1. 来自网络、配置或 shell 的数据都视为不可信输入。
2. 未认证输入只能触发廉价解析、认证尝试、丢弃或聚合 metric，不能创建 session、stream、topology、channel-plan state 或重传队列。
3. 非法版本、认证失败、长度越界、枚举非法、重放、乱序或格式错误不能依赖 panic 处理。
4. 可变长字段先验证长度再分配。

Panic 原则：

1. Panic 用来暴露实现 bug 和保留现场，不是输入校验机制。
2. Core 内部可以用 `assert`、`expect` 表达已经由类型、构造函数或前置校验保证的不变量。
3. 如果内部状态自相矛盾、进入理论不可达状态，或继续运行可能污染状态，panic 是合适的。
4. Core 不主动调用 `process::exit` 或主动 abort；但在 `panic = "abort"` 构建配置下，panic 会终止进程，这是部署策略的一部分。

Core 不实现复杂全局资源配额。设计假设攻击者不知道 PSK 或私钥；资源防护放在认证前廉价筛选、认证失败不建状态、解析前长度检查和自然 backpressure 上。

## 17. 简洁性与兼容性

RayNet 不承诺历史兼容性。系统部署模型假设所有节点可以一起升级。

1. 协议格式、配置格式、profile 和算法可以破坏性调整。
2. 节点遇到不匹配版本、profile、密码套件或能力集合时直接拒绝。
3. 主线不保留 legacy adapter、兼容分支、迁移 shim 或旧行为开关。
4. 能删除的代码优先删除；能用统一状态机表达的逻辑，不拆成隐式协作的后台 task。
5. 不为尚未真实存在的需求预留复杂扩展点。

## 18. 长期方向

长期方向包括：

1. `no_std + alloc`，让 core 能进入更受限的嵌入环境。
2. 更窄、更稳定的 C ABI，用于 C、Python、BEAM VM 等 shell。
3. 更多 transport channel，包括更适合高延迟批量信道的 mailbox/file、HTTP polling、GitHub upload/download 等。
4. 更丰富的 channel-plan 评分和调度策略，但不牺牲当前 core/shell 边界。
5. 面向不同网络形态的可靠传输 profile，例如低延迟交互、批量高延迟传输、弱连接恢复等。

这些方向不改变当前核心边界：core 是同步状态机，shell 负责运行时和 IO，relay 不理解 endpoint payload，entry 负责全局 channel-plan 调度。

## 19. 当前连接模型

Entry endpoint：

```text
ingress connector
  -> EndpointCore
  -> channel plan
  -> transport channel
```

Exit endpoint：

```text
transport channel
  -> EndpointCore
  -> exit connector
```

Relay：

```text
ingress transport
  -> RelayCore
  -> egress transport
```

入口当前支持 socks5，但 entry 不绑定 socks5 语义；后续可以加入 HTTP CONNECT、透明代理或平台特定 ingress。Exit 当前优先支持 TCP connect；后续可以加入 UDP associate 或其它 connector。
