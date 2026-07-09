# RayNet 设计文档

## 1. 当前目标

RayNet 是一个由 Rust core 和 runtime shell 共同组成的 overlay network。

Core 是同步状态机，负责协议状态、加密、KCP session、relay forwarding 和单向 route plan 调度。Shell 负责真实 IO、timer、配置加载、启动熵、日志、ingress connector 和 exit connector。

1. Entry endpoint 接收本地连接，把它变成一个 endpoint session。
2. Endpoint 发送 packet 时使用一个单向 `RoutePlan`。
3. Relay 消费 `RoutePlan` 的第一项并转发，endpoint payload 始终由 endpoint 处理。
4. Exit endpoint 为远端 session 打开真实服务连接，并把返回流量作为另一个方向独立发送。

传输 channel 可以是 UDP、TCP、HTTP、DNS、mailbox/file、GitHub upload/download 或其它 shell 能实现的东西。RayNet 通用配置不定义 channel kind。`ChannelId` 只是某个节点本地的 transport handle。

Channel 可以是有向的；`A -> B` 存在不代表 `B -> A` 存在。Entry -> exit 和 exit -> entry 是两个独立方向，可以使用不同 route、不同 MTU 和不同健康状态。

Relay 和 endpoint 的 channel table 在进程生命周期内固定。增删或重编号 channel 需要重启相关节点，并同步相关 endpoint 的 route 配置。

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
  opaque transport packet generation/parsing
  envelope authentication and encryption
  endpoint encryption
  KCP session
  route-plan forwarding
  channel health and scheduling
```

Core 只推进内存中的协议状态。Shell 把外部事件转成 `CoreEvent`，core 推进状态并输出 `CoreAction`，shell 执行动作。IO、系统时间、task、sleep、socket、文件、环境变量、网络 API 和系统随机源都属于 shell。

Core 需要随机性时，只使用构造配置里的 128-bit `RandomSeed` 派生内部 nonce 和随机流。Shell 负责每次启动时生成 seed；测试 shell 可以传固定 seed 获得可复现结果。

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

Entry 和 exit 是部署角色。两端都使用 `EndpointCore`。入口侧 shell 负责把本地 ingress 连接转换成 session；出口侧 shell 负责按 `OpenExitConnection` 连接真实服务。入口部署可以禁止执行 `OpenExitConnection`，以隔离 exit 节点被攻破后的反向风险。

Relay 的行为固定为：认证解开 packet，消费 `route_plan[0]`，按指定本地 channel 发出。

## 4. Core API

Rust 版本先使用 Rust-native API。

Core 实现以 Rust 为准。跨语言 shell 不重新实现协议逻辑，只通过 Rust core 的 API/ABI 驱动同一个状态机。

```text
EndpointCore::new(config) -> Result<EndpointCore, ConfigError>
RelayCore::new(config) -> Result<RelayCore, ConfigError>

core.handle_event(now, event, action_sink) -> Result<(), CoreError>
core.poll(now, action_sink) -> Result<(), CoreError>
core.next_deadline(now) -> Option<deadline>
```

`now` 是 shell 传入的虚拟单调时间。Core 不主动查询时间。Shell 在收到外部事件时调用 `handle_event`，在 `next_deadline` 到期时调用 `poll`。`action_sink` 是 core 写入输出动作的临时 sink，Rust 实现可以直接使用 `&mut Vec<CoreAction>`。

Core config 在构造时一次性传入，运行期不通过 event 修改结构性配置。

```text
EndpointConfig
  envelope_key
  message_key
  random_seed: [u8; 16]
  local_channels: Vec<ChannelId>
  route_topology: RouteTopology
  transport_mtu

RelayConfig
  envelope_key
  random_seed: [u8; 16]
  local_channels: Vec<ChannelId>
```

当前密钥模型只区分两类 key：

```text
envelope_key
  用于 envelope 认证加密
  EndpointCore 和 RelayCore 都持有

message_key
  用于 endpoint payload 端到端认证加密
  只有 EndpointCore 持有
```

Endpoint config 传入本 endpoint 发送方向可见的 `RouteTopology`。`RouteTopology` 是一张有向图，节点用 `NodeId` 表示，边用各节点本地的 `ChannelId` 表示。Core 根据这份方向拓扑生成或选择实际 `RoutePlan`。

```text
RouteTopology
  nodes:
    node_id
    channels:
      channel_id
      peer_node_id
```

`RouteTopology` 的起点是本 endpoint，终点是这张方向拓扑汇向的 endpoint。拓扑应收敛到一个明确终点。

`RoutePlan` 是 `Vec<ChannelId>`，由 endpoint core 从 `RouteTopology` 生成。一个 plan 只描述某个 packet 当前方向要走的本地 channel 序列。

## 5. 标识

```text
NodeId
  节点身份
  用于配置、结构化事件、测试和 endpoint 可见的有向拓扑图

ChannelId
  某个节点本地的 transport channel 编号
  在 route plan 中按当前节点的本地语义解释

SessionId
  endpoint session 编号
  一个 session 对应一条被代理的本地连接
  由发起侧 core 分配

LocalConnectionId
  endpoint shell 本地连接句柄
  只存在于 entry/exit endpoint

Target
  exit endpoint 要连接的目标描述
```

`LocalConnectionId` 对应 entry 侧 ingress connection 或 exit 侧真实服务连接。Relay 不使用 `LocalConnectionId`。

`SessionId` 是协议里的 KCP session id。Entry 和 exit 用它对应同一条被代理连接。Shell 可以把 `SessionId` 当 opaque token 回传给 core，但不生成或解释它。

## 6. CoreEvent

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
    session_id,
    local_connection_id
  }
  ExitConnectionOpenFailed {
    session_id,
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
```

Connection 相关事件只用于 `EndpointCore`。`RelayCore` 只处理 transport packet、channel 状态和 timer 推进。

`TransportChannelUpdated` 对 endpoint 有调度价值：endpoint 可以根据本地发送失败、队列压力和 KCP timeout 调整自己发送方向的 route plan。Relay 不利用它重路由；relay 发送失败等价于丢包，由 endpoint 的 KCP 重传恢复。

## 7. CoreAction

```text
CoreAction
  SendTransportPacket {
    channel_id,
    bytes
  }
  OpenExitConnection {
    session_id,
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
  EmitMetric(metric)
  EmitEvent(event)
```

Shell 执行 `SendTransportPacket` 时必须使用 core 指定的 `channel_id`。Shell 不解释 `bytes`，只把它当 opaque bytes 发给指定 channel。

Core 输出结构化 event。Shell 负责 event 的呈现、聚合、指标转换和 trace 接入。

## 8. Wire Protocol

```text
TransportPacket
  shell 收发的 opaque bytes
  外部观察者不应看到 magic、明文版本、明文 profile、明文类型、明文长度字段或固定协议标记

Envelope
  envelope 认证解开后的节点间转发单元

EndpointPayload
  endpoint 间端到端加密和认证的数据

SessionFrame
  KCP reassembly 后的单连接 frame
```

`TransportPacket` 是 core 输出给 shell 的实际发送字节。外层格式不使用 magic number、明文版本、明文 profile、明文 packet type、明文长度字段或其它固定协议标记。接收方只能用配置中的 `envelope_key` 尝试认证解密；失败就丢弃。

认证材料、nonce、tag、padding 和 ciphertext 在外观上应尽量接近随机字节，不暴露可稳定匹配的 RayNet 特征。外层 packet 可以加入随机长度 padding；padding 长度不以明文字段暴露。

认证解开后的内部结构可以使用明确长度字段和 payload。可变长字段必须先验证长度再分配；整数、长度、时间和索引字段必须定义字节序、单位、溢出行为和非法值处理。

Envelope 解开后包含：

```text
Envelope
  route_plan
  payload
```

Relay 只读取 `route_plan`。`payload` 对 relay 是 opaque bytes。

Endpoint payload 解密后包含：

```text
EndpointPayload
  session_id
  kcp_segment
```

KCP reassembly 后，endpoint 解析 `SessionFrame`：

```text
SessionFrame
  frame_type
  length
  payload

FrameType
  OpenConnection { target, metadata }
  ConnectionBytes { bytes }
  CloseConnection { reason }
  ResetConnection { reason }
  KeepAlive
```

Envelope auth 使用认证加密保护 envelope，防止外部伪造或篡改。Message encryption/auth 保护 endpoint payload，relay 不持有 endpoint payload 的解密能力。

当前威胁模型信任已认证 relay，不把 relay compromise 作为主要防御目标。Relay 可能看到 route plan，但看不到 endpoint payload。

## 9. KCP 与 Session

KCP 运行在 logical endpoint session 之间，作为端到端可靠传输机制，不绑定单个 link 或 channel。Relay 只转发 envelope，不理解 KCP segment、session id、exit target 或 endpoint payload。

一个 ingress TCP connection 对应一个 endpoint session，也对应一个 KCP session。Shell 可以在 ingress/exit connector 层把多条本地连接复用成一条逻辑连接；core 看到的仍然是一条 session。

一个 `EndpointCore` 可以同时维护多个 endpoint session。每个 session 独立持有 KCP 状态、重传队列和本地连接映射。收到已认证 endpoint payload 后，如果 `session_id` 尚不存在，core 可以按该 session 的第一批 KCP segment 创建对应 session 状态。

`SessionFrame` 位于 KCP reassembly 之后，只表达这条连接的 open、bytes、close、reset 和 keepalive，不重复定义可靠传输序列语义。

EndpointCore 必须知道自己发送方向的 `transport_mtu`，因为 KCP 要用 MTU/MSS 决定 segment 大小。Shell 不负责对 KCP segment 做可靠分片重组。如果 core 输出超大 transport packet，再由 shell 在外层随意拆分，任一外层分片丢失都会让接收端无法恢复该 KCP segment。

Entry -> exit 和 exit -> entry 可以使用不同的 transport MTU。每个 endpoint 只关心自己发送方向的 MTU；对端回程使用对端自己的 MTU 配置。

## 10. Channel 与 RoutePlan

每个节点启动时加载固定本地 channel table，也就是本节点可发送的 `ChannelId` 集合。`ChannelId` 只在所属节点的命名空间内有意义。真实 transport 目标由 shell 的 channel 实现管理。

Endpoint 的 `RouteTopology` 可以引用路径上其它节点的本地 `ChannelId`，因为发送 endpoint 负责生成该方向的完整 plan。

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

RelayB consumes RelayB_ch7
Envelope.route_plan = [RelayC_ch2]
SendTransportPacket { channel_id: RelayB_ch7, bytes }

RelayC consumes RelayC_ch2
Envelope.route_plan = []
SendTransportPacket { channel_id: RelayC_ch2, bytes }
```

接收 endpoint 收到 `route_plan` 为空的 envelope 后，把 payload 交给 endpoint session。

Relay 的正常转发规则只有一种：`route_plan` 非空时，消费第一项作为本地 outbound `ChannelId`，把剩余 plan 写回 envelope 并发送。

已知 channel 的真实发送失败由 shell 记录并通过后续事件反馈；对当前 packet 来说它就是丢包。

Relay 只维护本地 channel table 和必要的结构化事件。

## 11. 调度与 MTU

当前调度目标是简单可靠地绕开坏 channel。Endpoint 不维护每条完整 route 的 score；route 数量可能随拓扑指数级增长，按完整 route 记分会让状态膨胀。

Endpoint 在 `RouteTopology` 的边上维护运行时状态，并根据这些边状态生成 `RoutePlan`。某条 route 失败时，失败信号可以回写到该 route 经过的边；后续恢复信号可以逐步恢复相关边的状态。

第一版调度可以使用简单的 loss-based edge state。每条 topology 边附带少量运行时字段：

```text
loss_ewma
probe_budget
last_update
```

`loss_ewma` 是这条边的近期丢包/失败估计。`probe_budget` 用于让已经降权的边偶尔被低频探测，避免 channel 恢复后永远不用。`last_update` 使用虚拟单调时间，用于状态衰减和探测节奏。

Endpoint 生成 `RoutePlan` 时，不枚举所有 route；它在 `RouteTopology` 上按边成本求一条低成本路径。第一版成本函数可以很简单：

```text
edge_cost = base + loss_weight * loss_ewma + down_penalty
route_cost = sum(edge_cost)
```

`down_penalty` 只表达本地已知 channel down 或连续硬失败。没有 hard down 时，主要由 `loss_ewma` 决定选路。

发送失败、KCP timeout 或重传压力上升时，把该 packet 经过的边的 `loss_ewma` 往上推。成功 ACK、稳定发送完成或探测成功时，把相关边的 `loss_ewma` 往下拉。更新方式使用 EWMA，避免单次波动立刻改变全局选择：

```text
on_failure: loss_ewma = loss_ewma * (1 - alpha) + alpha
on_success: loss_ewma = loss_ewma * (1 - beta)
```

`alpha` 可以大于 `beta`，让坏 channel 被更快避开，恢复则更慢进入主路径。

如果某条边连续失败，`loss_ewma` 会快速接近高值，route generation 会自然避开包含该边的路径。如果 channel 恢复，低频 probe 成功后会逐步降低 `loss_ewma`，让该边重新参与正常选路。

后续 route generation 和 edge-state 更新算法仍需要单独调研。目标是：

1. channel 突然断开或质量明显变差时，能较快避开相关边。
2. channel 恢复后，能通过低风险探测逐步重新使用。
3. 状态量随 topology 边数增长，不随 route 数量爆炸。
4. 后续可以从 KCP、send error、queue pressure 等处增加更多信号，但第一版只以 loss EWMA 为主。
5. 后续评估 EWMA、AIMD、multi-armed bandit、BBR 类估计等思路。

Relay 只执行 envelope 中 `route_plan` 的第一项指令。

MTU 是 endpoint core config 中发送方向的 transport MTU。Endpoint 用它配置 KCP MSS，并预留外层认证和最大 padding 的开销，避免 `SendTransportPacket.bytes` 超过 shell 承诺的承载上限。

最大接收包长是资源保护上限。当前使用编译期常量：

```text
MAX_TRANSPORT_PACKET_SIZE = 64 KiB
```

relay 和 endpoint 收到超过该上限的 transport packet，在认证和分配大缓冲前直接丢弃。

## 12. Time、Nonce 与 Replay

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

Envelope auth、nonce filter、replay 窗口和 KCP timer 都使用 shell 传入的同一个虚拟单调时间。

Nonce filter 是内存型短期 replay protection。窗口内重复 nonce 会被拒绝；窗口外 replay 依赖 envelope auth 的时间关联失败。

## 13. Panic 与安全

输入边界：

1. `TransportPacketReceived.bytes` 是网络进入 core 的不可信输入。
2. 未认证 transport bytes 只能触发廉价解析、认证尝试、丢弃或聚合 metric，不能创建 session、topology、route-plan state 或重传队列。
3. 网络包的认证失败、长度越界、枚举非法、重放、乱序或格式错误不能依赖 panic 处理。
4. 网络包里的可变长字段先验证长度再分配。

Panic 原则：

1. Panic 用来暴露实现 bug 和保留现场，不是输入校验机制。
2. 构造函数、认证、解密和基础长度校验完成后，core 可以按设计不变量已经成立来实现，不需要在每个内部步骤重复做防御性检查。
3. Core 内部可以用 `assert`、`expect` 表达已经由类型、构造函数或前置校验保证的不变量。
4. Core 不主动调用 `process::exit` 或主动 abort；但在 `panic = "abort"` 构建配置下，panic 会终止进程，这是部署策略的一部分。

Core 不实现复杂全局资源配额。设计假设攻击者不知道 PSK 或私钥；资源防护放在认证前廉价筛选、认证失败不建状态、解析前长度检查和自然 backpressure 上。

## 14. 简洁性与兼容性

RayNet 不承诺历史兼容性。系统部署模型假设所有节点可以一起升级。

1. 协议格式、配置格式和算法可以破坏性调整。
2. 来自网络的 packet 如果格式、密钥、密码套件或能力不匹配，直接丢弃。
3. 已进入 core 内部状态的不变量不做层层兼容校验；预期不可能发生的状态按实现 bug 处理。
4. 主线不保留 legacy adapter、兼容分支、迁移 shim 或旧行为开关。
5. 能删除的代码优先删除；能用统一状态机表达的逻辑，不拆成隐式协作的后台 task。
6. 不为尚未真实存在的需求预留复杂扩展点。

## 15. 当前连接模型

Entry endpoint：

```text
ingress connector <-> EndpointCore
EndpointCore -> route plan -> transport channel -> EndpointCore
```

Exit endpoint：

```text
EndpointCore <-> exit connector
EndpointCore -> route plan -> transport channel -> EndpointCore
```

Relay：

```text
ingress transport -> RelayCore -> egress transport
```

入口可以由 socks5、HTTP CONNECT、透明代理或平台特定 ingress 提供目标信息；entry core 不绑定具体 ingress 语义。Exit 当前优先支持 TCP connect；后续可以加入 UDP associate 或其它 connector。

## 16. 长期方向

长期方向包括：

1. `no_std + alloc`，让 core 能进入更受限的嵌入环境。
2. 更窄、更稳定的 C ABI，用于 C、Python、BEAM VM 等 shell。
3. 更多 transport channel，包括更适合高延迟批量信道的 mailbox/file、HTTP polling、GitHub upload/download 等。
4. 更成熟的 edge-state 和 route generation 策略，但不牺牲当前 core/shell 边界。
5. 面向不同网络形态的可靠传输参数，例如低延迟交互、批量高延迟传输、弱连接恢复等。

这些方向不改变当前核心边界：core 是同步状态机，shell 负责运行时和 IO，relay 不理解 endpoint payload，endpoint 负责自己发送方向的 route-plan 调度。
