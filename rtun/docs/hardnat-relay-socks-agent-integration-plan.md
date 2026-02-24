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

## Milestone 3：`relay + agent` 的 UDP relay 高难 NAT fallback（第一条生产路径）

### 目标

在 `UdpRelay` 建链中增加 `fallback` 模式：ICE 失败后尝试高难 NAT 打洞；默认关闭。

### 待办事项

- [ ] 定义 `UdpRelay` 的建链编排层（例如 `open_udp_relay_tunnel_with_fallback`）
- [ ] 保留现有 `open_udp_relay_tunnel(...)` 作为 ICE 主路径实现
- [ ] 仅在 `hard_nat.mode=fallback` 时启用 fallback 分支
- [ ] relay 侧：ICE 失败条件分类（仅对可重试/无选中地址触发 fallback）
- [ ] agent 侧：`handle_udp_relay(...)` 增加 hard-nat server 角色支持（不影响 ICE 默认路径）
- [ ] 实现 token/session 关联，避免串会话误判
- [ ] 成功后统一返回同样的 UDP 通道抽象给上层（上层不用知道来源）
- [ ] 失败时保留原有错误并追加 hard-nat 失败上下文
- [ ] 增加独立日志前缀（建议 `[relay_hardnat_diag]` / `[agent_hardnat_diag]`）

### 验收项

- [ ] 默认模式（off）下，`relay` 行为完全不变
- [ ] `fallback` 开启后，普通网络场景 ICE 成功，hard-nat 不干扰
- [ ] 人工构造 ICE 失败场景时，能看到 fallback 尝试日志
- [ ] fallback 成功时，UDP relay 能持续转发（用 `udp echo` + `udp_probe_local.py` 验证）

### 人工检查点（必须停）

1. 重点评审资源释放（socket/task 是否泄漏）
2. 检查日志是否能明确区分 `ICE fail` / `HardNAT fail`
3. 长跑验证（至少一轮）
4. 确认后进入 Milestone 4

---

## Milestone 4：`socks + agent` 的 QUIC socks 高难 NAT fallback

### 目标

把同样的 fallback 能力集成到 `socks` 的 QUIC 打洞路径。

### 待办事项

- [ ] 为 `QuicSocks`（`P2PQuicArgs`）增加 hard-nat 配置承载（若 Milestone 2 未做）
- [ ] `cmd_socks` 增加 CLI 参数（默认 `off`）
- [ ] `quic_pool.rs` 的 `punch(...)` 外层增加 fallback 编排
- [ ] agent 侧 `handle_quic_socks(...)` 增加 hard-nat server 角色支持
- [ ] 成功后仍按现有流程 `upgrade_to_quic(...)`，不改 QUIC 上层逻辑
- [ ] 增加 `winner=ice|hardnat` 日志/指标

### 验收项

- [ ] `socks` 默认配置行为不变
- [ ] `fallback` 模式在实验环境下可触发并完成 QUIC 建链
- [ ] QUIC 建链成功后 SOCKS 数据面可用（基本回归）

### 人工检查点（必须停）

1. 审查 QUIC 升级前后的 socket 生命周期是否正确
2. 验证 fallback 成功后 QUIC 层没有额外副作用
3. 确认后进入 Milestone 5

---

## Milestone 5：assist 模式（并联尝试，先成功者获胜）

### 目标

增加 `assist` 模式，在高难 NAT 场景降低建链时延，但不影响默认行为。

### 待办事项

- [ ] 设计并实现“first success wins”并发编排（ICE vs HardNAT）
- [ ] 增加延迟启动参数（例如 hard-nat 延迟 X ms 启动）
- [ ] 实现 loser 分支取消和资源清理
- [ ] 明确 winner 记录与日志
- [ ] 增加竞态测试（双成功/同时失败/一方超时）

### 验收项

- [ ] `assist` 模式在普通 NAT 场景不显著增加资源占用/日志噪音
- [ ] 高难 NAT 场景下相比 `fallback` 有更快成功样本（至少实验数据）
- [ ] 无明显资源泄漏

### 人工检查点（必须停）

1. 重点审查并发取消与资源释放
2. 对比 `off/fallback/assist` 三种模式日志和性能
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
- [ ] Milestone 3 未开始
- [ ] Milestone 4 未开始
- [ ] Milestone 5 未开始
- [ ] Milestone 6 未开始

> 执行提醒：每完成一个 milestone，先更新本文件中的勾选状态，再停下来人工检查确认。
