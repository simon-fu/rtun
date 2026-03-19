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
7. nat3 针对目标 `nat4_ip` 随机取一批端口，对所有端口发送探测包。
8. nat4 如果有 socket 收到探测包，则将该 socket 的 TTL 设置为正常值，回复 ack 响应，并进入握手阶段。
9. nat4 处在握手阶段的 socket 连续发送 `N1` 个包并收到 nat3 的响应后，则认为连接稳定，尝试抢占候选者。
10. nat4 抢到候选者的 socket 进入候选者状态，其他 socket 进入握手等待状态。
11. nat4 候选者状态 socket 连续发送 `N2` 个包并收到 nat3 的响应后，进入连接成功状态，其他 socket 释放资源，打洞成功结束。
12. nat4 候选者状态如果超时没有建立连接，则释放候选者，所有握手等待状态的 socket 重复步骤 9。
13. 如果步骤 8 没有 socket 收到探测包，或当前 `nat4_ip` 这一轮未建立连接，则切换到下一个 `nat4_ip`，重复步骤 6。
14. 如果步骤 6 已经取完所有 IP，则从 `nat4_candidate_ips[]` 第一个 IP 再次开始，进入下一轮 IP 轮转。
15. 如果当前 `nat3_addr` 下完成 `N3` 轮 IP 轮转后仍未建立连接，则重复步骤 4。
16. 如果步骤 4 已经取完所有地址，则从 `nat3_public_addrs[]` 第一个地址再次尝试。
17. 如果步骤 4 的外层轮转尝试达到 `N4` 次后仍未建立成功，则打洞失败结束。

## 实现硬约束

- 后续 `relay/agent` 实现必须以本流程为准，不能跳过关键收敛步骤。
- `Connected` 只能由 UDP 数据面收敛达成，核心路径必须严格经过：`ack -> N1 -> 候选者 -> N2`。
- `nat4` 可以作为唯一调度者，负责切换端口批次、`nat4_ip`、`nat3_addr`、续租和中止。
- 控制面只能做调度，不能越过 UDP 数据面，直接把任一方推进到“已连接”。
- 如果实现里出现“未经过 `ack -> N1 -> 候选者 -> N2`，仅凭控制消息就宣布 `Connected`”，则视为不符合本算法。

## 说明

- nat3 使用单个 UDP socket 执行探测，并通过 `nat3_public_addrs[]` 向 nat4 暴露可尝试的公网地址。
- nat4 使用一批 UDP socket 执行 TTL warm up 和接收，只有命中的 socket 才进入握手和候选者竞争。
- probing 阶段使用较小 TTL，只有在步骤 8 命中后才恢复为正常 TTL，避免把低 TTL 带入后续数据面。

## nat3 状态机

nat3 侧需要与上面的 nat4 主流程配合，最小状态机定义如下：

- `Idle`
  尚未开始打洞，没有选定当前 `nat4_ip` 和端口批次。
- `Probing`
  正在对当前 `nat4_ip` 的一批随机端口发送 `probe` 包。
- `Handshake`
  已经收到 nat4 对某个 5-tuple 的 `ack`，停止对当前批次的随机扫端口，只保留该 5-tuple 的往返握手。
- `Connected`
  已经与 nat4 候选者 socket 建立稳定连通，进入成功状态。
- `Failed`
  外层尝试达到 `N4` 上限，打洞失败结束。

状态切换规则：

1. nat3 初始进入 `Idle`，选定当前 `nat4_ip` 和端口批次后进入 `Probing`。
2. nat3 在 `Probing` 状态下，如果收到 nat4 对某个 5-tuple 的 `ack`，则记住该远端地址与本地 socket 组合，停止当前批次的随机端口探测，进入 `Handshake`。
3. nat3 在 `Handshake` 状态下，只对触发 `ack` 的那个 5-tuple 继续收发握手包，不再并行向其它随机端口继续探测。
4. nat3 在 `Handshake` 状态下，如果持续收到来自 nat4 候选者 socket 的握手响应并满足成功条件，则进入 `Connected`。
5. nat3 在 `Handshake` 状态下，如果超时没有继续收到有效响应，则回退到外层重试流程，由 nat4/nat3 继续执行步骤 13 到步骤 17。
6. nat3 在外层尝试达到 `N4` 上限后进入 `Failed`。

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

## 资源生命周期

### nat3 资源

- nat3 在整个会话中复用同一个 UDP socket。
- `nat3_public_addrs[]` 由这个 socket 的 STUN 结果导出，后续 `probe`、`ack` 响应和握手都使用同一个 socket。
- nat3 在同一个 `nat3_addr` 下切换 `nat4_ip`、进入下一轮 IP 轮转、或切换 `nat3_addr` 时，都不重建本地 socket。
- nat3 只有在进入 `Connected` 或 `Failed` 后才释放该 socket。

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

## 报文定义

本文档只定义最小语义，不限定最终编码格式。至少需要三类报文：

- `probe`
  由 nat3 在步骤 7 发出，用于对当前 `nat4_ip` 的随机端口做探测。
- `ack`
  由 nat4 在步骤 8 命中后回复，用于把 nat3 从 `Probing` 推进到 `Handshake`。
- `handshake`
  由 nat3 和 nat4 在步骤 9 到步骤 12 之间双向收发，用于确认候选者和最终连通。

最小字段要求：

1. 所有报文都应至少带 `session_id`，防止并发会话互相串包。
2. `ack` 和 `handshake` 应能让接收方识别当前命中的 nat4 socket，至少要能区分当前 5-tuple 或等价的 socket 标识。
3. `handshake` 应包含足够的阶段信息，使 nat3 能识别“普通握手响应”和“候选者确认响应”。

最小判定规则：

1. nat3 只有在 `Probing` 状态收到与当前会话匹配的 `ack` 时，才切换到 `Handshake`。
2. nat4 只有在收到与当前会话匹配的 `probe` 后，才允许进入步骤 8。
3. nat3 和 nat4 在 `Handshake` 阶段只接受当前已锁定 5-tuple 上的 `handshake` 报文，其它来源一律忽略。
4. 一旦某个 nat4 socket 成为候选者，后续只有该候选者 socket 的 `handshake` 响应可以推进到成功状态，其它 socket 只能维持等待或被淘汰。
