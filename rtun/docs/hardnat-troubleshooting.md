# HardNAT 排障说明

本文档聚焦 `relay / agent / socks` 中 HardNAT 相关日志定位与常见失败原因分析。

## 先判断是不是 HardNAT 路径问题

先看配置日志：

- `mode=off`：当前不是 HardNAT 数据面问题（仍是 ICE-only 路径）
- `mode=assist`：需要分别看 ICE 与 HardNAT 两路结果
- `mode=force`：当前问题主要落在 HardNAT 路径

常用 grep 关键词：

- `relay_hardnat_diag`
- `agent_hardnat_diag`
- `winner=ice`
- `winner=hardnat`
- `both ICE and hard-nat failed`
- `hard-nat planning`

## 日志定位路径（按建链阶段）

### 1. relay 侧：配置与模式判定

看：

- `udp relay hard-nat config`
- `udp relay hard-nat ... enabled`

用于确认：

- 当前 `mode/role`
- 参数是否是预期值（含 `assist_delay`)
- 是否仍处于 placeholder 路径（如 `fallback`）

### 2. relay/agent 侧：规划阶段（基于 ICE 候选）

看：

- `udp relay hard-nat planning`

用于确认：

- `parsed_candidates`
- `usable_udp_candidates`
- `nat3_target_ip`
- `nat4_target`
- `local_role` / `remote_role`

如果这里 `nat3_target_ip` 或 `nat4_target` 为 `None`，HardNAT 无法继续，优先检查 ICE 候选内容。

### 3. assist 竞态阶段（若 mode=assist）

看：

- `winner=ice`
- `winner=hardnat`
- `both ICE and hard-nat failed`

用于判断：

- HardNAT 只是“备胎未赢”，还是确实也失败
- ICE 失败时 HardNAT 是否有补偿成功

### 4. force 阶段（若 mode=force）

看：

- `hard-nat ... force enabled`
- `hard-nat planning`
- 最终 `connected` 或错误日志

用于判断：

- 是否成功完全绕开 ICE 数据面
- 错误发生在目标规划、socket 建立还是探测阶段

## 常见失败原因与验证方法

### A. ICE 候选不足/不可用，导致 HardNAT 无目标

现象：

- `parsed_candidates` 很少或 `usable_udp_candidates=0`
- `nat3_target_ip=None` / `nat4_target=None`

原因：

- 对端没有可用 UDP 候选
- 候选字符串异常/解析失败
- 候选里只有不适合的地址

验证：

1. 打开更详细日志，确认完整 ICE candidates
2. 对照 `hard-nat planning` 输出检查被过滤情况
3. 先用 `mode=off` 验证 ICE-only 是否本身就不稳定

### B. 参数过激进导致资源问题或无效探测

现象：

- 启动时报参数错误（relay/socks）
- agent 侧 warning 提示参数被 clamp

原因：

- `socket_count/scan_count/interval/ttl` 超过安全上限

验证：

1. 看启动错误信息（relay/socks CLI 会拒绝启动）
2. 看 agent 日志是否有 `values were clamped`
3. 先恢复默认值再逐项调参

### C. assist 双失败（ICE + HardNAT 都失败）

现象：

- `both ICE and hard-nat failed`
- 日志里同时带两个错误原因

原因（常见）：

- 网络真实不可达 / NAT 行为限制
- ICE 失败，且 HardNAT 目标规划也不合适
- relay/agent 两端配置不一致（模式、角色、参数）

验证：

1. 先记录两个错误（不要只看最终一条）
2. 切换到 `force` 只测 HardNAT
3. 切回 `off` 只测 ICE
4. 用 `udp echo + udp_probe_local.py` 做固定流量验证

### D. assist 总是 `winner=ice`

现象：

- HardNAT 未赢，但业务正常

解释：

- 这不一定是问题；说明当前网络下 ICE 足够快/稳定

需要做的：

1. 看是否存在 HardNAT 失败日志（没有则可接受）
2. 若想更积极测试 HardNAT，可适当调小/调大 `assist_delay`
3. 不要仅因 `winner=ice` 就认为 HardNAT 有 bug

### E. force 模式失败，但 off 模式正常

现象：

- `off` 正常
- `force` 失败

解释：

- 说明默认 ICE 路径是可用的，HardNAT 路径仍需调试
- 这不代表默认产品路径回归

验证：

1. 看 `hard-nat planning` 的目标是否合理
2. 检查 `role` 是否需要显式指定 `nat3/nat4`
3. 对比同一时段 `assist` 模式下 HardNAT 是否偶发成功

## 建议排障顺序（避免混淆根因）

1. `off` 建立基线（ICE-only）
2. `assist` 观察 winner 与双失败日志
3. `force` 单独验证 HardNAT
4. 仅在明确需要时调大 `socket_count/scan_count`
5. 记录时间点、agent instance、两端日志并对齐分析

## 当前实现边界（避免误判）

- `UDP relay fallback` 仍未实现（占位日志）
- `socks(quic)` HardNAT 业务路径仍未实现（当前只有参数透传和日志占位）
- 因此看到相关 placeholder 日志是预期行为，不是异常

