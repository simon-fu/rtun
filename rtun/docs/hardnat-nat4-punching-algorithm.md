# HardNAT NAT4 打洞流程算法

本文档记录 `nat4 <-> nat3` 场景下的 NAT4 打洞流程，用于统一 issue、调试和后续实现中的算法描述。

## 角色与记号

- `nat3_public_addrs[]`
  nat3 创建单个 UDP socket 后，通过 STUN 获取到的公网地址列表，包含端口。
- `nat4_candidate_ips[]`
  nat4 创建一批 UDP socket 后，通过 STUN 获取到的公网地址列表，去重 IP 后得到的公网 IP 列表。
- `N1`
  nat4 握手阶段用于确认连接稳定的连续收发轮次阈值。
- `N2`
  nat4 候选者阶段用于确认最终连通的连续收发轮次阈值。
- `N3`
  nat3 在同一个 `nat3_addr` 下，对 `nat4_candidate_ips[]` 完整轮转的最大轮次数；每轮中每个 `nat4_ip` 只打一批随机端口。
- `N4`
  nat4 对 `nat3_public_addrs[]` 外层轮转的总尝试次数上限。

## 打洞流程

1. 一端是 nat4，另一端是 nat3。
2. nat3 创建一个 UDP socket，通过 STUN 获取外网地址列表，记作 `nat3_public_addrs[]`。
3. nat4 创建一批 UDP socket，通过 STUN 获取外网地址列表，去重 IP，得到 IP 列表，记作 `nat4_candidate_ips[]`。
4. nat4 从 `nat3_public_addrs[]` 里取出下一个地址 `nat3_addr`。
5. nat4 以 `nat3_addr` 为目标地址，所有 socket 执行 TTL warm up 到指定 TTL 值，然后进入接收状态。
6. nat3 从 `nat4_candidate_ips[]` 里取出下一个 IP `nat4_ip`。
7. nat3 针对当前 `nat4_ip` 随机取一批端口，对所有端口持续发送 `probe_req`，作为本轮 `nat4_ip` 轮次的探测起点。
8. nat4 如果有 socket 收到 `probe_req`，则将该 socket 的 TTL 设置为正常值，回复 `probe_ack`，并进入握手阶段；nat3 收到 `probe_ack` 后只记录日志，仍保持 `Probe` 状态。
9. nat4 处在握手阶段的 socket 持续发送 `handshake_req`；nat3 收到后记录日志，并回复带 `tuple` 的 `handshake_ack`。nat4 依据收到的 `handshake_ack` 统计 `N1`，判断该 socket 是否具备抢占候选者的资格。
10. nat4 抢到候选者的 socket 进入候选者状态，其他 socket 进入握手等待状态；nat3 不在本地维护候选者状态，也不在本地选择 winner。
11. nat4 候选者状态 socket 连续发送 `N2` 个 `handshake_req` 并收到 nat3 的 `handshake_ack` 后，进入连接成功状态。此时 nat4 通过控制面把选中的 `tuple` 通知 nat3，nat3 收到后进入 `Connected`，打洞成功结束。
12. nat4 候选者状态如果超时没有建立连接，则释放候选者，所有握手等待状态 socket 重复步骤 9，在当前 `nat4_ip` 内继续竞争，直到候选者窗口耗尽。
13. 如果步骤 8 没有 socket 收到 `probe_req`，或当前 `nat4_ip` 的尝试窗口达到 `ip_try_timeout`、全体 socket 都退回 probing、或候选者窗口再无可继续推进的 socket，则 nat4 切换到下一个 `nat4_ip`，重新执行步骤 6。
    每个 `nat4_ip` 轮次以一次固定批次的 `probe_req` 扫描开始，持续等待候选者完成 `N2`。只有当 `nat4_candidate_ips[]` 从最后一个 IP 回到第一个 IP 时，才算完成一次完整 IP 轮转，并令 `N3` 计数递增。
14. 如果步骤 6 已经取完所有 IP，则从 `nat4_candidate_ips[]` 第一个 IP 再次开始，进入下一轮 IP 轮转。
15. 如果当前 `nat3_addr` 下完成 `N3` 轮 IP 轮转后仍未建立连接，则重复步骤 4。
16. 如果步骤 4 已经取完所有地址，则从 `nat3_public_addrs[]` 第一个地址再次尝试。
17. 如果步骤 4 的外层轮转尝试达到 `N4` 次后仍未建立成功，则打洞失败结束。

## 实现硬约束

- 后续 `relay/agent` 实现必须以本流程为准，不能跳过关键收敛步骤。
- UDP 数据面统一使用 JSON 编码，包类型固定为：`probe_req`、`probe_ack`、`handshake_req`、`handshake_ack`。
- `nat4` 是唯一的收敛方和 winner 选择方；只有 nat4 可以执行 `N1 -> 候选者 -> N2` 并最终确定选中的 `tuple`。
- nat4 的 `Connected` 必须先由 UDP 数据面收敛达成，核心路径必须严格经过：`probe_ack -> N1 -> 候选者 -> N2`。
- `nat3` 的协议状态只有 `Probe` 和 `Connected`。nat3 不在本地维护 `Handshake`/`Candidate` 状态，不在本地做 winner 选择。
- 控制面可以在 nat4 已完成 UDP 数据面收敛后，把选中的 `tuple` 通知 nat3，由 nat3 进入 `Connected`；但控制面不能在 nat4 尚未完成 `N1 -> 候选者 -> N2` 前，直接把 nat4 或 nat3 推进到“已连接”。
- 如果实现里出现“nat4 未经过 `probe_ack -> N1 -> 候选者 -> N2`，仅凭控制消息就宣布 `Connected`”，则视为不符合本算法。

## 说明

- nat3 使用单个 UDP socket 执行探测，并通过 `nat3_public_addrs[]` 向 nat4 暴露可尝试的公网地址。
- nat4 使用一批 UDP socket 执行 TTL warm up 和接收，只有命中的 socket 才进入握手和候选者竞争。
- probing 阶段使用较小 TTL，只有在步骤 8 命中后才恢复为正常 TTL，避免把低 TTL 带入后续数据面。

## nat3 状态机

nat3 侧采用简化状态机，只保留两个协议状态：

- `Probe`
  正在对当前 `nat4_ip` 的一批随机端口持续发送 `probe_req`，并处理来自 nat4 的数据面回包。
- `Connected`
  已经收到 nat4 通过控制面下发的选中 `tuple`，进入成功状态。

状态切换规则：

1. nat3 会话开始后直接进入 `Probe`。
2. nat3 在 `Probe` 状态下，持续对当前 `nat4_ip` 的当前端口批次发送 `probe_req`。
3. nat3 在 `Probe` 状态下，收到 `probe_ack` 后只记录日志，不切状态，不在本地锁定 winner。
4. nat3 在 `Probe` 状态下，收到 `handshake_req` 后记录日志，并回复 `handshake_ack`；该 `handshake_ack` 必须携带 nat3 当前认知的 `tuple`。
5. nat3 在 `Probe` 状态下，不维护本地 `N1`/`N2` 计数器，不维护本地 `Candidate`，也不停止 `probe_req`。
6. nat4 在本地完成 `N1 -> 候选者 -> N2` 后，通过控制面发送选中的 `tuple` 给 nat3；nat3 收到后进入 `Connected`。
7. 超时、租约失效、`Abort` 等均视为会话失败结果，不额外定义为 nat3 协议状态。

## 计数器语义

为避免实现时对 `N1`、`N2`、`N3`、`N4` 理解不一致，约定如下：

- `N1`
  单个 nat4 socket 进入握手阶段后，要求该 socket 连续发送并收到响应的最小轮次数。`N1` 只用于“该 socket 是否有资格抢候选者”的判定。
- `N2`
  nat4 候选者 socket 抢占成功后，要求该 socket 连续发送并收到响应的最小轮次数。`N2` 只用于“候选者是否最终确认连接成功”的判定。
- `N3`
  针对单个 `nat3_addr`，nat3 对 `nat4_candidate_ips[]` 做完整轮转的最大次数。每轮里，每个 `nat4_ip` 只生成一批随机目标端口并尝试一次；未通则立刻切到下一个 `nat4_ip`。
- `N4`
  nat4 外层地址轮转总次数上限。这里的“一次”指完成对 `nat3_public_addrs[]` 的一整轮遍历；每轮中，每个 `nat3_addr` 都会按步骤 6 到步骤 15 依次尝试所有 `nat4_candidate_ips[]`。

计数器重置规则：

1. `N1` 在某个 socket 首次进入握手阶段时清零；该 socket 退出握手阶段后失效。
2. `N2` 在某个 socket 成为候选者时清零；候选者被释放后失效。
3. `N3` 在完成一次 `nat4_candidate_ips[]` 全量轮转后递增一次；开始下一轮轮转前不清零。
4. `N3` 在切换到新的 `nat3_addr` 时清零，因为此时开始针对新的 `nat3_addr` 重新轮转整张 `nat4_candidate_ips[]`。
5. `N4` 只在完成一次 `nat3_public_addrs[]` 全量遍历后递增一次；开始下一轮遍历前不清零。

### N1/N2 计数规则

1. `N1`/`N2` 指“某个 nat4 socket 在同一次 candidate 流程中连续收发的 `handshake_req`/`handshake_ack` 次数”；每收到一次合法 `handshake_ack`，计数递增一次。
2. `handshake_ack` 必须匹配 `accepted_tuple`、`handshake_stage` 与 `generation`，否则被忽略；重复或乱序的 `handshake_ack`（seq 小于等于上次计数的 seq）不再计数。
3. 只要在当前 candidate 流程内有一次 `handshake_req`/`handshake_ack` 未按预期返回或超时（由 nat4 控制判断），当前 candidate 流程计数视为失败，`N1`/`N2` 计数清零，nat4 递增 `generation` 后可重新发起候选者尝试。
4. `pre_candidate` 阶段的计数不能带入 `candidate` 阶段；当 nat4 成功抢到候选者后，N1 计算停止，开始按同一 candidate 流程追踪 `N2`。
## 资源生命周期

### nat3 资源

- nat3 在整个会话中复用同一个 UDP socket。
- `nat3_public_addrs[]` 由这个 socket 的 STUN 结果导出；nat3 通过它持续发送 `probe_req`，并通过同一个 socket 接收来自 nat4 的 `probe_ack`/`handshake_req`，然后回复 `handshake_ack`。
- nat3 在同一个 `nat3_addr` 下切换 `nat4_ip`、进入下一轮 IP 轮转、或切换 `nat3_addr` 时，都不重建本地 socket。
- nat3 只有在会话结束后才释放该 socket；会话结束包括成功、超时、租约失效、`Abort` 等。

### nat4 资源

- nat4 在步骤 3 创建一批 UDP socket，并在整个会话中尽量复用这批 socket。
- nat4 在步骤 5 对整批 socket 执行一次针对当前 `nat3_addr` 的 TTL warm up，然后进入接收状态。
- 在同一个 `nat3_addr` 下切换 `nat4_ip` 或重新从第一个 `nat4_ip` 开始下一轮轮转时，nat4 继续复用当前这批 socket 和当前 `nat3_addr` 的接收状态，不重新建 socket，也不重新做 warm up。
- 在步骤 15 切换到新的 `nat3_addr` 时，nat4 继续复用同一批 socket，但需要重新以新的 `nat3_addr` 为目标执行 TTL warm up，再进入接收状态。
- 进入步骤 8 的命中 socket 会先恢复为正常 TTL，再进入握手阶段。
- 未命中的 socket 保持 probing TTL。
- 某个候选者 socket 在步骤 12 超时失效后，该 socket 退出候选者状态；其余握手等待状态 socket 继续参与候选者竞争。
- nat4 在进入 `Connected` 后，只保留最终成功的那个 socket，其余 socket 释放。
- nat4 在进入 `Failed` 后，释放整批 socket。
当某个 socket 命中过但最终未完成 `N1`/`N2`（比如切换 `nat4_ip`、切换 `nat3_addr`、候选者失败，或控制面超时）时，必须立即：
  1. 让该 socket 恢复 probing TTL；
  2. 清除 `accepted_tuple`/`candidate` 状态；
  3. 递增 `generation`（即便是同一 socket）后重新参与下一轮 `probe_req`/`handshake_req`；
  4. 保证 `N1`/`N2` 计数从头开始，不跨 candidate 传递。

## 报文定义

本版协议固定使用 JSON 编码，UTF-8 文本载荷。所有 UDP 包都遵循统一信封格式：

```json
{
  "v": "hn1",
  "packet_type": "probe_req",
  "session_id": 123456,
  "sender": {
    "role": "nat3",
    "socket_id": 0,
    "generation": 0,
    "seq": 1
  }
}
```

公共字段定义：

- `v`
  协议版本字符串。当前固定为 `hn1`。
- `packet_type`
  包类型。当前固定为：`probe_req`、`probe_ack`、`handshake_req`、`handshake_ack`。
- `session_id`
  会话 ID。用于隔离并发会话，防止串包。
- `sender.role`
  发送方角色，取值为 `nat3` 或 `nat4`。
- `sender.socket_id`
  发送方 socket 标识。nat3 当前只有 1 个 socket，固定使用 `0`；nat4 使用自身 socket 的稳定标识。
- `sender.generation`
  发送方当前代际。nat3 固定使用 `0`；nat4 在普通握手阶段可使用 `0`，进入候选者竞争后使用当前候选代际。
- `sender.seq`
  发送方序号。按发送方本地递增，用于日志、去重和匹配。

### generation 语义

- `generation == 0` 表示 nat4 仍处于 pre_candidate/普通握手阶段。
- 当某个 nat4 socket 成功抢占 candidate 并进入 `N1 -> 候选者 -> N2` 流程后，nat4 必须为该 candidate 分配一个新的 `generation > 0`，并在随后的 `handshake_req`/控制面 `Connected` 中携带。
- 候选者失败、当前 candidate 被释放、同一 socket 再次参与 candidate、nat4 切换 `nat4_ip` 或 nat3 切换 `nat3_addr` 导致当前 candidate 失效时，都必须递增 `generation`，确保 nat3 只能根据最新 `generation` 校验状态。

扩展字段定义：

- `handshake_stage`
  仅用于 `handshake_req` 和 `handshake_ack`。取值为 `pre_candidate` 或 `candidate`。
  `pre_candidate` 表示 nat4 仍处于候选者竞争前的普通握手阶段；
  `candidate` 表示 nat4 已经选定候选者，正在做最终确认。
- `ack_of`
  仅用于 `handshake_ack`。表示当前 `handshake_ack` 是对哪个 `handshake_req.sender` 的确认。
- `tuple`
  仅用于 `handshake_ack`，以及 nat4 通过控制面通知 nat3 的最终 `Connected` 结果。
  结构固定如下：

```json
{
  "nat3_addr": "1.2.3.4:50000",
  "nat4_addr": "8.8.8.8:40000"
}
```

其中：

- `tuple.nat3_addr`
  nat3 当前控制游标下正在尝试的 `nat3_addr`。这是 nat3 在当前轮中认为自己对应的公网地址。
- `tuple.nat4_addr`
  nat3 收到本次 `handshake_req` 时观察到的远端 `IP:port`，即当前参与握手的 nat4 socket 地址。

四类包的最小格式如下：

1. `probe_req`

```json
{
  "v": "hn1",
  "packet_type": "probe_req",
  "session_id": 123456,
  "sender": {
    "role": "nat3",
    "socket_id": 0,
    "generation": 0,
    "seq": 17
  }
}
```

2. `probe_ack`

```json
{
  "v": "hn1",
  "packet_type": "probe_ack",
  "session_id": 123456,
  "sender": {
    "role": "nat4",
    "socket_id": 11,
    "generation": 0,
    "seq": 3
  }
}
```

3. `handshake_req`

```json
{
  "v": "hn1",
  "packet_type": "handshake_req",
  "session_id": 123456,
  "sender": {
    "role": "nat4",
    "socket_id": 11,
    "generation": 2,
    "seq": 9
  },
  "handshake_stage": "candidate"
}
```

4. `handshake_ack`

```json
{
  "v": "hn1",
  "packet_type": "handshake_ack",
  "session_id": 123456,
  "sender": {
    "role": "nat3",
    "socket_id": 0,
    "generation": 0,
    "seq": 41
  },
  "ack_of": {
    "role": "nat4",
    "socket_id": 11,
    "generation": 2,
    "seq": 9
  },
  "handshake_stage": "candidate",
  "tuple": {
    "nat3_addr": "1.2.3.4:50000",
    "nat4_addr": "8.8.8.8:40000"
  }
}
```

字段约束：

1. 所有 UDP 包都必须带 `v`、`packet_type`、`session_id`、`sender`。
2. `probe_req` 和 `probe_ack` 不带 `handshake_stage`、`ack_of`、`tuple`。
3. `handshake_req` 必须带 `handshake_stage`，用于区分普通握手和候选者确认。
4. `handshake_ack` 必须带 `ack_of` 和 `tuple`。
5. nat3 收到 `handshake_req` 时，必须把该 `handshake_req.sender.role/socket_id/generation/seq` 原样填入 `handshake_ack.ack_of`，并填入自己当前认知的 `tuple`。
6. 对于同一个 nat4 socket 的同一轮 `N1` 或 `N2` 统计，第一条被接受的 `handshake_ack` 会定义该轮的 `accepted_tuple`；后续计数只接受 `tuple` 与 `accepted_tuple` 且 `generation` 完全一致的 `handshake_ack`。
7. `tuple` 可接受的最小条件如下：
   - `tuple.nat3_addr` 必须等于当前调度轮中的目标 `nat3_addr`
   - `tuple.nat4_addr.ip` 必须等于当前正在尝试的 `nat4_ip`
   - `tuple.nat4_addr` 必须等于 nat3 收到本次 `handshake_req` 时观察到的远端 `IP:port`
8. nat4 在统计 `N1` 和 `N2` 时，只认会话匹配、`ack_of` 匹配、`tuple` 可接受的 `handshake_ack`。

## 控制面 Connected 结果

nat4 在本地完成 `N1 -> 候选者 -> N2` 后，必须通过控制面把最终选中的结果通知 nat3。最小字段如下：

```json
{
  "session_id": 123456,
  "selected_socket_id": 11,
  "selected_generation": 2,
  "tuple": {
    "nat3_addr": "1.2.3.4:50000",
    "nat4_addr": "8.8.8.8:40000"
  },
  "restore_ttl": 64
}
```

字段定义：

- `session_id`
  当前 hard-nat 会话 ID。
- `selected_socket_id`
  nat4 最终胜出的 socket 标识。
- `selected_generation`
  nat4 最终胜出 socket 对应的候选代际。
- `tuple`
  nat4 最终确认成功的 tuple。
- `restore_ttl`
  nat4 winner socket 进入连接态后应恢复使用的 TTL。

nat3 侧最小校验规则：

1. `session_id` 必须匹配当前会话。
2. nat3 只有在本地仍处于 `Probe` 状态时，才接受该 `Connected` 结果。
3. nat3 必须在 UDP 数据面先观察到 `handshake_stage == candidate` 的 `handshake_req`，并基于该 `handshake_req.sender.socket_id/generation` 记录最新的 candidate event。
4. `tuple.nat3_addr` 必须属于当前会话的 `nat3_public_addrs[]`。
5. `tuple.nat4_addr` 必须等于 nat3 在上述 `handshake_req` 中观察到的远端 `IP:port`，并与 `selected_socket_id`/`selected_generation` 一致。
6. `selected_socket_id` 和 `selected_generation` 必须能与 nat3 已记录的上述 candidate `handshake_req.sender.socket_id/generation` 对应上，且 `tuple` 与该 candidate event 完全一致；只有同时满足所有这三项时，nat3 才将控制面 `Connected` 视为合法，否则记录日志并忽略。
7. 任一校验失败时，nat3 只能记录诊断日志并忽略该 `Connected` 结果，不能切到 `Connected`。

最小判定规则：

1. nat4 只有在收到与当前会话匹配的 `probe_req` 后，才允许回复 `probe_ack` 并进入握手阶段。
2. nat3 在 `Probe` 状态下收到 `probe_ack` 后只记录日志，不切状态。
3. nat3 在 `Probe` 状态下收到 `handshake_req` 后，必须回复 `handshake_ack`，但不在本地选择 winner。
4. nat4 只有在收到足够多的 `handshake_ack` 后，才允许推进 `N1`、候选者和 `N2`。
5. nat4 完成 `N2` 后，必须通过控制面把最终选中的 `tuple` 发给 nat3，nat3 才能进入 `Connected`。
