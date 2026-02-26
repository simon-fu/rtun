# HardNAT 运维说明（relay / socks / agent）

本文档对应 `rtun` 的 HardNAT 增强打洞能力（当前以 `relay udp relay` 为主，`socks(quic)` 仍为参数/日志占位）。

## 适用范围（当前实现状态）

- `relay` UDP relay:
  - `off`: 已实现（默认）
  - `force`: 已实现
  - `assist`: 已实现（并联 ICE 与 HardNAT，先成功者获胜）
  - `fallback`: 未实现（占位日志）
- `socks` QUIC:
  - HardNAT 参数透传与日志占位已实现
  - `force/assist/fallback` 业务路径未实现（仍走 ICE-only）

## 自动策略结论（Milestone 6）

本期不实现“基于候选特征/历史失败模式”的动态 `role=auto` 自动决策，原因：

- 缺少稳定的实网样本与失败标签，误判风险高
- 当前 `assist/force` 路径刚落地，优先保证可观测性与手动可控
- 动态策略一旦误判，会放大资源消耗与排障复杂度

当前保留的 `--p2p-hardnat-role auto` 含义是“静态角色规划”：

- initiator 默认用 `nat3`
- responder 默认用 `nat4`

未来若启用动态 auto，回退策略建议：

- 仅在 `assist/force` 显式开启时生效（默认路径不受影响）
- 单次会话动态 auto 失败后自动降级到静态 `role=auto`
- 若连续失败达到阈值，自动降级到 `mode=off`（或人工配置回退）

## 参数说明（relay / socks）

以下参数命名在 `relay` 与 `socks` 保持一致（`socks` 当前主要用于占位与日志验证）。

- `--p2p-hardnat <off|fallback|assist|force>`
- `--p2p-hardnat-role <auto|nat3|nat4>`
- `--p2p-hardnat-socket-count <N>`
- `--p2p-hardnat-scan-count <N>`
- `--p2p-hardnat-interval <ms>`
- `--p2p-hardnat-batch-interval <ms>`
- `--p2p-hardnat-assist-delay <ms>`（`relay` 当前生效；`socks` 未实现 assist 路径）
- `--p2p-hardnat-ttl <N>`
- `--p2p-hardnat-no-ttl`

## 默认值与安全上限

默认值（当前实现）：

- `socket_count = 64`
- `scan_count = 64`
- `interval = 1000ms`
- `batch_interval = 5000ms`
- `assist_delay = 300ms`
- `ttl = auto`（或 `--p2p-hardnat-no-ttl` 禁用预发送）

安全上限（Milestone 6 收敛，防止误用）：

- `socket_count <= 1024`
- `scan_count <= 4096`
- `interval <= 60000ms`
- `batch_interval <= 300000ms`
- `assist_delay <= 10000ms`
- `ttl <= 255`

行为说明：

- `relay/socks` CLI 超出上限会直接报错拒绝启动
- `agent` 收到超限 proto 参数时会记录 warning，并对值做 clamp（防止远端误配置放大资源消耗）

## 推荐配置（起步方案）

### 1. 默认稳定优先（推荐）

- `--p2p-hardnat off`

适用：

- 大多数普通 NAT 环境
- 先排查基础网络/心跳问题，避免引入额外变量

### 2. UDP relay 诊断增强（推荐）

- `--p2p-hardnat assist`
- `--p2p-hardnat-assist-delay 300`

适用：

- ICE 可用但偶发失败，希望保留 ICE 成功率同时试探 HardNAT

建议：

- 先保持默认 `socket_count/scan_count`
- 观察 `winner=ice|hardnat` 分布和失败日志

### 3. 强制 HardNAT 诊断（仅实验/排障）

- `--p2p-hardnat force`

适用：

- 明确需要验证 HardNAT 路径本身行为

风险：

- 会绕过 ICE 数据面建立路径，结果不代表默认路径稳定性

## 启用与回滚流程（UDP relay）

启用（assist）：

1. 在 `relay` 增加 `--p2p-hardnat assist`
2. 保持默认参数跑一轮观察日志
3. 确认 `winner` 与失败原因可定位后，再考虑调参数

回滚（最快路径）：

1. 改回 `--p2p-hardnat off`
2. 保留其他参数不影响行为（`off` 下不会写入 hard-nat proto）

## 日志与观测（当前实现）

建议重点 grep：

- `relay_hardnat_diag`
- `agent_hardnat_diag`
- `winner=ice`
- `winner=hardnat`
- `both ICE and hard-nat failed`

关键日志语义：

- 配置日志：当前 mode/role/参数值
- 规划日志：从 ICE 候选推导的 hard-nat 目标
- winner 日志：assist 竞态谁赢
- 双失败日志：assist 两路都失败（同时带 ICE 与 hard-nat 错误）
- clamp 日志（agent）：收到超限配置，已降到安全上限

## 调试步骤（UDP relay）

### A. 确认默认路径正常（基线）

1. `--p2p-hardnat off`
2. 跑现有业务流或 `udp echo + udp_probe_local.py`
3. 确认无异常后再启用 `assist/force`

### B. assist 观察竞态结果

1. 开 `--p2p-hardnat assist`
2. 看 `relay_hardnat_diag` / `agent_hardnat_diag` 的 `winner`
3. 若双失败，记录：
   - ICE 失败原因
   - HardNAT 失败原因
   - 时间点与 agent instance

### C. force 仅验证 hard-nat 路径

1. 开 `--p2p-hardnat force`
2. 用固定流量（如 UDP echo）连续探测
3. 不要直接用 force 结果替代默认路径稳定性判断

## 测试矩阵（Milestone 6）

当前建议最小矩阵（先 UDP relay，后 socks）：

| 维度 | 模式 | 期望 | 当前状态 |
|---|---|---|---|
| UDP relay | `off` | 纯 ICE 路径稳定 | 已实现 |
| UDP relay | `assist` | ICE 与 HardNAT 并联，先成功者获胜 | 已实现 |
| UDP relay | `force` | 强制 HardNAT 路径可建链 | 已实现 |
| UDP relay | `fallback` | ICE 失败后再试 HardNAT | 未实现（占位） |
| socks(quic) | `off` | 纯 ICE 路径稳定 | 已有路径 |
| socks(quic) | `assist/force/fallback` | 参数透传+日志明确 | 占位（未实现业务路径） |

建议每个已实现模式至少覆盖：

- 启动配置日志正确
- 成功路径日志（winner/connected）
- 失败路径日志可定位
- 参数越界时拒绝或 clamp 符合预期

