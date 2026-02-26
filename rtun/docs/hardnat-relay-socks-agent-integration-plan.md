# 高难 NAT 打洞集成计划（relay / socks / agent）

## 目标

在 **不改动现有 P2P 打洞逻辑（ICE 主路径）** 的前提下，把 `nat4` 子命令中的高难 NAT 打洞思路以增量方式集成到以下生产路径中：

1. `relay`（UDP relay）
2. `socks`（QUIC socks 打洞）
3. `agent`（`OpenP2P` 的服务端处理）

方案要求：

1. 现有 ICE 路径保持默认行为不变。
2. 新增“高难 NAT 打洞”能力，默认关闭，通过参数启用。
3. 先做 `fallback`（ICE 失败后再尝试），后续再做 `assist`（并联）。
4. 每个 milestone 完成后必须停下来人工检查，确认后再继续。

## 背景与现状（当前代码）

### 现有生产打洞路径（保持不动）

1. `relay` UDP relay 建链入口：
   - `rtun/src/bin/rtun/cmd_relay/cmd_relay.rs` 中 `open_udp_relay_tunnel(...)`
2. `socks` QUIC 打洞入口：
   - `rtun/src/bin/rtun/cmd_socks/quic_pool.rs` 中 `punch(...)`
3. `agent` OpenP2P 处理：
   - `rtun/src/switch/agent/ctrl.rs` 中 `handle_udp_relay(...)`
   - `rtun/src/switch/agent/ctrl.rs` 中 `handle_quic_socks(...)`
4. ICE 核心逻辑：
   - `rtun/src/ice/ice_peer.rs` (`client_gather/server_gather/dial/accept`)

### `nat4` 子命令现状（实验/工具性质）

1. 入口：
   - `rtun/src/bin/rtun/cmd_nat4/cmd_nat4.rs`
2. 包含两个角色：
   - `nat3-send`（单 socket + 随机端口扫描）
   - `nat4-send`（多 socket + 固定目标发送 + 可选 TTL 预发）
3. 特征：
   - 更像高难 NAT 场景的 UDP 打洞实验工具
   - 不是标准 ICE 流程的一部分
   - 不是 NAT 类型判定器

## 总体方案（冻结版本）

### 设计原则

1. **不改现有 ICE 逻辑**
   - 不修改 `IcePeer` 的既有 `client_gather/server_gather/dial/accept` 行为。
2. **新增编排层**
   - 在 `relay/socks/agent` 调用点外增加“高难 NAT 打洞编排层”。
3. **先 fallback，后 assist**
   - 第一阶段只做 `ICE -> hardnat fallback`。
   - 第二阶段再做 `ICE + hardnat` 并联尝试。
4. **默认关闭**
   - 新能力必须显式开启。

### 运行模式（计划）

1. `off`（默认）
   - 仅使用现有 ICE 路径。
2. `fallback`
   - ICE 失败后进入高难 NAT 打洞。
3. `assist`（后续里程碑）
   - ICE 先启动，高难 NAT 延迟并联；谁先成功用谁。
4. `force`（可选调试模式）
   - 跳过 ICE，直接尝试高难 NAT（仅用于实验/调试）。

## 协议与参数策略（规划）

### 协议扩展原则

1. 在现有 `OpenP2P` 协议基础上做 **可选字段** 扩展。
2. 老版本不带字段时，行为完全不变。
3. 对端不支持时，按 `off` 或回退到 ICE-only。

### 计划新增协议结构（示意）

1. 新增 `P2PHardNatArgs`（optional）
   - `mode`：`off|fallback|assist|force`
   - `role`：`auto|nat3|nat4`
   - `socket_count`
   - `scan_count`
   - `interval_ms`
   - `batch_interval_ms`
   - `ttl_mode`
   - `ttl`
   - `window_ms`
   - `token`（会话关联/防串会话）
2. 挂载位置（计划）
   - `UdpRelayArgs`
   - `P2PQuicArgs`

注：第一阶段可以先只支持 `UdpRelayArgs`，`QuicSocks` 后续复用。

## 模块化重构策略（不改行为的前提下）

### 拆分方向

1. 把 `cmd_nat4` 的可复用逻辑抽到库模块（例如 `rtun/src/p2p/hard_nat.rs`）
2. `cmd_nat4` 继续作为 CLI 包装层（调用库模块）
3. 生产路径（relay/socks/agent）调用同一库模块

### 统一抽象（建议）

定义统一的“已打通 UDP 通道”抽象（名称待定）：

1. `P2pUdpConn`
   - `socket`
   - `remote_addr`
   - `meta`（`winner=ice|hardnat`、耗时、模式等）

这样上层不需要关心成功来自 ICE 还是高难 NAT。

## 风险与约束（必须先控制）

1. 资源风险
   - 多 socket、多包探测可能放大 CPU / FD / 带宽占用。
2. 并发风险
   - 同一 session 中并联策略可能产生竞态与资源泄漏。
3. 误接入风险
   - 探测包必须带 token/session 关联，避免串包误判成功。
4. 兼容风险
   - 协议扩展必须向后兼容。
5. 观测不足风险
   - 必须新增独立日志前缀，明确 `ICE fail` vs `HardNAT fail`。

## 执行规则（必须遵守）

1. 每个 milestone 完成后：
   - 勾选该 milestone 下已完成待办项
   - 运行该 milestone 对应的验证项
   - **停止继续开发**
   - 等待人工检查与确认
2. 只有在人工确认后，才开始下一个 milestone。
3. 如果 milestone 验收失败：
   - 不进入下一 milestone
   - 先修复本 milestone 问题并重新验收
4. 每个 milestone 建议单独提交 commit（便于回滚和评审）。

## Milestones 与待办事项

---

## Milestone 0：方案冻结与接口边界确认（仅文档/设计）

### 目标

冻结范围、默认行为、参数/协议方向，避免实施中途反复改设计。

### 待办事项

- [ ] 确认高难 NAT 集成仅作为增量能力，不改现有 ICE 主路径逻辑
- [ ] 确认第一阶段只做 `fallback`，默认关闭
- [ ] 确认优先覆盖 `relay + agent` 的 `UdpRelay`
- [ ] 确认 `socks/QuicSocks` 放在后续 milestone
- [ ] 确认是否需要 `force` 调试模式（可选）
- [ ] 确认日志与指标最小集合（成功/失败/耗时/winner）

### 人工检查点（必须停）

1. 评审本计划文档是否符合目标
2. 明确第一阶段范围与不做项
3. 确认后进入 Milestone 1

---

## Milestone 1：抽取 `nat4` 为库模块（行为不变）

### 目标

把 `cmd_nat4` 的核心能力抽成可复用库模块，但 `nat4` 子命令行为保持一致（回归基线）。

### 待办事项

- [x] 识别 `cmd_nat4` 中可复用逻辑（角色、发包、收包、TTL 预发、成功判定）
- [x] 新建库模块（例如 `rtun/src/p2p/hard_nat.rs`）
- [x] 抽出公共配置结构（role/mode/socket_count/scan_count/interval/ttl/token 等）
- [x] 抽出运行结果结构（成功/失败原因/耗时/命中地址）
- [x] 让 `cmd_nat4` 改为调用库模块（CLI 参数与输出兼容）
- [x] 保留 `nat4` 现有 usage 方式可运行
- [x] 为库模块补最小单元测试（参数校验/状态机/编码 token）

### 验收项

- [x] `rtun nat4 nat3-send ...` 行为与原来一致（手工 smoke）
- [x] `rtun nat4 nat4-send ...` 行为与原来一致（手工 smoke）
- [x] 无 `relay/socks/agent` 代码路径行为变化

### 人工检查点（必须停）

1. 代码审查：确认只是抽模块，没有引入生产路径逻辑变化
2. 手工跑一组 `nat3/nat4` 验证命令
3. 确认后进入 Milestone 2

---

## Milestone 2：协议扩展（可选字段，默认不生效）

### 目标

为 `OpenP2P` 增加高难 NAT 配置字段，但默认路径仍然完全走 ICE。

### 待办事项

- [x] 在 `rtun/proto/app.proto` 新增 `P2PHardNatArgs`（optional）
- [x] 在 `UdpRelayArgs` 挂载 `hard_nat` 可选字段
- [ ] （可选）在 `P2PQuicArgs` 挂载 `hard_nat` 可选字段（若阶段内准备同步做 socks）
- [x] 重新生成 proto 代码（若工程生成流程需要）
- [x] 在 relay 构造 `UdpRelayArgs` 时支持填充字段（默认不填）
- [x] 在 agent 解析 `UdpRelayArgs` 时安全读取字段（不存在则 `off`）
- [x] 增加兼容性测试/断言（字段缺失不改变行为）

### 验收项

- [ ] 新旧版本不带 `hard_nat` 字段时，`relay` 行为与日志无变化
- [ ] agent 能正确解析“字段缺失/字段为默认值”
- [ ] 不开启参数时不会触发任何 hard-nat 日志

### 人工检查点（必须停）

1. 协议兼容性评审（字段编号、默认值、回退行为）
2. 回归测试 `relay/agent` 基本建链
3. 确认后进入 Milestone 3

---

## Milestone 3：`relay + agent` 的 UDP relay 高难 NAT 参数/日志占位（不实现 fallback）

### 目标

在 `UdpRelay` 建链路径中补齐 hard-nat 的参数入口与诊断日志占位，默认关闭；本里程碑不实现 fallback 打洞逻辑。

### 待办事项

- [x] `cmd_relay` 增加 hard-nat CLI 参数（默认 `off`）
- [x] relay 侧把 CLI 参数映射到 `UdpRelayArgs.hard_nat`（仅传参，不改建链分支）
- [x] relay 侧增加 hard-nat 配置日志（mode/role/socket_count/scan_count/ttl 等）
- [x] agent 侧增强 hard-nat 参数解析与诊断日志（收到非 `off` 时打印明确告警/占位日志）
- [x] 保持 `open_udp_relay_tunnel(...)` 与 `handle_udp_relay(...)` 的 ICE 主路径行为不变
- [x] 增加参数解析/默认值单测（至少 `off`、`fallback`、`role`、`ttl/no_ttl`）
- [x] 增加日志前缀约定（建议 `[relay_hardnat_diag]` / `[agent_hardnat_diag]`）
- [x] 在文档中明确：实际 fallback 实现延后到后续里程碑

### 验收项

- [ ] 默认模式（off）下，`relay` 行为完全不变
- [ ] 传入 hard-nat 参数时，relay/agent 两侧能看到一致的配置/占位日志
- [ ] 设置 `hard_nat.mode=fallback` 时，不触发实际 fallback，只出现明确“未实现/占位”日志
- [ ] 基本 UDP relay 建链/转发回归不受影响

### 人工检查点（必须停）

1. 审查 CLI 参数默认值与 proto 映射是否一致
2. 检查占位日志是否足够明确（不会误导为已启用 fallback）
3. 回归一轮 UDP relay 基本链路（确认无行为变化）
4. 确认后进入 Milestone 4（再决定是否恢复/插入 relay fallback 实现里程碑）

---

## Milestone 4：`socks + agent` 的 QUIC socks 高难 NAT 参数/日志占位（不实现 fallback）

### 目标

在 `socks` 的 QUIC 打洞路径中补齐 hard-nat 的参数入口与诊断日志占位，默认关闭；本里程碑不实现 fallback 打洞逻辑。

### 待办事项

- [x] 为 `QuicSocks`（`P2PQuicArgs`）增加 hard-nat 配置承载
- [x] `cmd_socks` 增加 hard-nat CLI 参数（默认 `off`）
- [x] `quic_pool.rs` 的 `punch(...)` 把 hard-nat 参数映射到 `P2PQuicArgs.hard_nat`（仅传参，不改建链分支）
- [x] `quic_pool.rs` 增加 hard-nat 配置日志与建链占位日志（明确仍走 ICE-only）
- [x] agent 侧 `handle_quic_socks(...)` 增加 hard-nat 参数解析与占位诊断日志
- [x] 保持 `punch(...)` / `handle_quic_socks(...)` 的 ICE + QUIC 主路径行为不变
- [x] 增加参数解析/默认值单测（至少 `off`、`fallback`、`role`、`ttl/no_ttl`）
- [x] 增加日志前缀约定（建议 `[socks_hardnat_diag]` / `[agent_hardnat_diag]`）
- [x] 在文档中明确：QUIC hard-nat fallback 实现延后到后续里程碑

### 验收项

- [ ] `socks` 默认配置行为不变
- [ ] 传入 hard-nat 参数时，socks/agent 两侧能看到一致的配置/占位日志
- [ ] 设置 `hard_nat.mode=fallback` 时，不触发实际 fallback，只出现明确“未实现/占位”日志
- [ ] QUIC 建链与 SOCKS 数据面基本回归不受影响

### 人工检查点（必须停）

1. 审查 `cmd_socks` CLI 默认值与 `P2PQuicArgs.hard_nat` 映射是否一致
2. 检查 `socks/agent` 占位日志是否足够明确（不会误导为已启用 fallback）
3. 回归一轮 QUIC socks 基本链路（确认无行为变化）
4. 确认后进入 Milestone 4.5（先完成 hard-nat 可复用执行层前置改造）

---

## Milestone 4.5：`hard_nat` 可复用执行层前置改造（一次性执行 API）

### 目标

为后续 `relay/socks/agent` 的 `fallback/assist` 编排提供可复用的 hard-nat 执行层接口：支持“一次性打洞并返回已选中 socket/远端地址”，避免沿用当前 CLI 风格的成功后无限发送循环。

### 待办事项

- [x] 在 `p2p::hard_nat` 中新增一次性执行 API（例如 `run_nat3_once` / `run_nat4_once`）
- [x] 返回可复用连接信息（至少 role、local_addr、remote_addr、elapsed，以及可继续使用的 socket 句柄）
- [x] 将当前 `run_nat3` / `run_nat4` 改为对一次性 API 的包装，保持 `nat4` CLI 行为不变（成功后继续发送）
- [x] 确保探测接收任务在一次性 API 返回前停止/清理，避免与后续数据面抢读同一 socket
- [x] 增加最小单测（校验一次性 API 相关辅助逻辑/返回结构或清理路径）

### 验收项

- [ ] `rtun nat4 nat3` / `rtun nat4 nat4` CLI 行为与当前保持一致
- [ ] 一次性 API 能为后续编排层提供“可接管 socket”的返回值（无后台探测任务残留）
- [ ] 不引入 `relay/socks/agent` 行为变化（本里程碑不接入生产路径）

### 人工检查点（必须停）

1. 审查一次性 API 返回结构是否足以支撑后续 fallback/assist 编排
2. 检查探测任务清理策略是否会与后续数据面抢占 socket
3. 手工 smoke `nat4 nat3/nat4`（确认 CLI 行为未变）
4. 若当前无实网环境，可先以“本地 UDP 集成测试通过 + 实网 smoke 待补”方式人工确认并继续
5. 确认后进入 Milestone 4.6（hard-nat 角色/目标规划工具）

---

## Milestone 4.6：`hard_nat` 角色/目标规划工具（基于 ICE 候选）

### 目标

在库层沉淀 hard-nat 的“角色分配”和“目标选择”规划逻辑（基于 `P2PHardNatArgs` + `IceArgs.candidates`），避免在 `relay/socks/agent` 三处各自解析 ICE 候选并做不一致的策略判断。

### 待办事项

- [x] 在 `p2p::hard_nat` 中增加 role hint 解析与本端角色推导工具（考虑 initiator/server 视角）
- [x] 在 `p2p::hard_nat` 中增加基于 `IceArgs.candidates` 的候选解析与目标选择工具（优先公网 UDP 候选）
- [x] 给出稳定的选择结果结构（至少能提供 `nat3` 目标 IP 与 `nat4` 目标 `SocketAddr`）
- [x] 增加单测覆盖：`auto`/显式 role hint、候选优先级、忽略无效候选/非 UDP 候选
- [x] 保持生产路径不变（本里程碑仅提供规划工具）

### 验收项

- [ ] 规划工具在给定 ICE 候选集合时能稳定输出目标与角色
- [ ] 单测覆盖关键分支（role 推导、候选优先级、降级路径）
- [ ] 不引入 `relay/socks/agent` 行为变化

### 人工检查点（必须停）

1. 审查 role hint 语义是否清晰（特别是“同一 proto 字段在两端的解释方式”）
2. 审查候选优先级是否符合预期（srflx/host/relay 等）
3. 确认后进入 Milestone 4.7（UDP relay `force` 单通路前置里程碑）

---

## Milestone 4.7：UDP relay `force` 单通路（不含 assist/fallback）

### 目标

在不改动默认行为的前提下，为 `relay + agent` 的 `UdpRelay` 接入真正的 hard-nat 数据面建链路径（仅 `mode=force` 生效）。保留现有 ICE 候选交换逻辑，用于 hard-nat 角色/目标规划；`off/fallback/assist` 仍保持当前行为（其中 `fallback/assist` 继续占位）。

### 待办事项

- [x] 让 `p2p::hard_nat` one-shot API 在 future 被取消/超时时不会泄漏内部 `recv` task（至少 `abort`）
- [x] relay 侧 `open_udp_relay_tunnel(...)` 增加 `mode=force` 分支，使用 hard-nat one-shot 建立 tunnel socket
- [x] agent 侧 `handle_udp_relay(...)` / `udp_relay_task(...)` 增加 `mode=force` 分支，使用 hard-nat one-shot 建立 tunnel socket
- [x] 保持 `off/fallback/assist` 行为不变（`fallback/assist` 仍为占位日志）
- [x] 增加/运行针对性验证（至少 `cargo check` + `hard_nat` 单测 + relay/agent hard-nat 参数相关单测）

### 验收项

- [ ] `mode=force` 已具备真实 hard-nat 建链路径（不再是 placeholder）
- [ ] `off/fallback/assist` 不发生行为变化
- [ ] one-shot API 取消路径不会遗留长期运行的探测 `recv` task（代码审查结论）
- [ ] 针对性编译/单测通过

### 人工检查点（必须停）

1. 审查 relay/agent 两侧 `mode=force` 分支条件与日志，确认不会误伤默认路径
2. 若有环境，手工验证一轮 `UDP relay + --p2p-hardnat force`；若无环境，记录为实网 smoke 待补
3. 确认后进入 Milestone 5（assist 模式）

---

## Milestone 5：assist 模式（先做 UDP relay，并联尝试先成功者获胜）

### 目标

先在 `relay + agent` 的 `UdpRelay` 路径实现 `assist` 模式（并联 `ICE` 与 `HardNAT`，先成功者获胜），在高难 NAT 场景降低建链时延，但不影响默认行为。`socks` 的 `assist` 仍留待后续里程碑。

### 待办事项

- [x] 设计并实现“first success wins”并发编排（ICE vs HardNAT）【UDP relay】
- [x] 增加延迟启动参数（例如 hard-nat 延迟 X ms 启动）【`assist_delay_ms` / `--p2p-hardnat-assist-delay`】
- [x] 实现 loser 分支取消和资源清理（future drop 取消；hard-nat one-shot 内部 recv task 有 guard）
- [x] 明确 winner 记录与日志（relay/agent 双端 `winner=ice|hardnat`）
- [x] 增加竞态测试（双成功/同时失败/一方超时，覆盖 `race_assist(...)` helper）

### 验收项

- [ ] `assist` 模式在普通 NAT 场景不显著增加资源占用/日志噪音（待实网 smoke）
- [ ] 高难 NAT 场景下相比 `fallback` 有更快成功样本（至少实验数据，待实网）
- [ ] 无明显资源泄漏（待实网长跑；当前代码审查/单测通过）

### 人工检查点（必须停）

1. 重点审查并发取消与资源释放（relay/agent 两侧 assist winner/loser 路径）
2. 对比 `off/fallback/assist` 三种模式日志与行为（本里程碑仅 UDP relay）
3. 确认后进入 Milestone 6

---

## Milestone 6：自动策略（可选）与文档收尾

### 目标

在已有稳定实现基础上，增加可选自动策略，并补齐文档/运维说明。

### 待办事项

- [ ] 评估是否需要 `role=auto`（基于候选特征/历史失败模式）
- [ ] 若实现 `auto`，明确误判回退策略（失败自动降级到 fallback/off）
- [ ] 增加运维文档：参数说明、推荐配置、调试方法
- [ ] 增加排障文档：常见失败原因与日志定位路径
- [ ] 增加端到端测试矩阵（off/fallback/assist）
- [ ] 收敛默认值和资源上限（防止误用）

### 验收项

- [ ] 文档完整，运维可独立启用/排障
- [ ] 自动策略开启时不会破坏默认路径稳定性
- [ ] 关键场景测试可复现

### 人工检查点（必须停）

1. 最终设计评审（默认值、风险、上线策略）
2. 确认全部 milestone 关闭
3. 准备发布/灰度

---

## 参数草案（初稿，后续在 Milestone 2/3/4 冻结）

### relay（建议）

- `--p2p-hardnat <off|fallback|assist|force>`
- `--p2p-hardnat-role <auto|nat3|nat4>`
- `--p2p-hardnat-socket-count <N>`
- `--p2p-hardnat-scan-count <N>`
- `--p2p-hardnat-interval <ms>`
- `--p2p-hardnat-batch-interval <ms>`
- `--p2p-hardnat-ttl <N>`
- `--p2p-hardnat-no-ttl`

### socks（建议）

与 `relay` 命名保持一致，减少认知成本。

## 观测与排障要求（实施时必须落实）

1. 每次建链打印：
   - mode、role、winner、耗时
2. 失败打印：
   - ICE 失败原因
   - HardNAT 失败原因
   - 最终失败归因
3. 关键计数（至少日志可统计）：
   - `ice_success`
   - `hardnat_success`
   - `fallback_entered`
   - `assist_winner_ice`
   - `assist_winner_hardnat`

## 当前状态（维护区）

- [x] Milestone 0（方案文档）已创建
- [x] Milestone 1 实施完成（人工检查通过）
- [ ] Milestone 2 实施完成（待人工检查）
- [x] Milestone 3 实施完成（人工检查通过）
- [x] Milestone 4 实施完成（人工检查通过）
- [x] Milestone 4.5 实施完成（人工检查通过；已补本地 UDP 集成测试，`nat4 nat3/nat4` 实网 smoke 待补）
- [x] Milestone 4.6 实施完成（人工检查通过）
- [x] Milestone 4.7 实施完成（人工检查通过）
- [x] Milestone 5 实施完成（人工检查通过；已完成 UDP relay assist，实网 smoke 待补）
- [ ] Milestone 6 未开始

> 执行提醒：每完成一个 milestone，先更新本文件中的勾选状态，再停下来人工检查确认。
