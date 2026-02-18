# rtun

## Build musl target
```bash
sudo yum install gcc
rustup target add x86_64-unknown-linux-musl
cargo build --release --target=x86_64-unknown-linux-musl
```

## Usage
```bash
# 服务
rtun agent listen --addr 0.0.0.0:8888 --https-key /certs/xxx.com.key --https-cert /certs/xxx.com.pem --secret sec123

# 出口方
rtun agent pub "https://xxx.com:8888" --agent rtun --expire_in 60 --secret sec123

# 入口方
rtun socks --listen 0.0.0.0:2080 "https://xxx.com:8888" --secret sec123
```

## Relay Subcommand

`relay` 已实现，用于 UDP 端口通过底层 P2P 通道转发（不走现有 QUIC 隧道）。

示例：

```bash
rtun relay \
  -L udp://0.0.0.0:15353?to=8.8.8.8:53 \
  -L 0.0.0.0:15354?to=8.8.4.4:53 \
  "quic://127.0.0.1:8888" \
  --secret sec123 \
  --quic-insecure
```

规则与参数：

- 子命令：`rtun relay`
- 规则参数：`-L [proto://]<listen_addr>?to=<target_addr>`
- `proto` 省略时默认 `udp`
- `-L` 可重复，用于多条规则
- 当前仅支持 `udp`（`tcp` 语法预留）
- `target_addr` 仅支持 `IP:PORT`（不支持域名）
- `--agent`：agent 名称正则，默认 `.*`
- `--udp-idle-timeout`：UDP flow 空闲超时（秒），默认 `120`
- `--udp-max-payload`：UDP 负载上限（字节），默认自动计算

行为说明：

- 信令支持 `https://` 和 `quic://`
- 转发使用底层 ICE P2P UDP 数据通道
- agent 选择按 `expire_at` 降序，优先选择过期时间最晚的实例
- 运行中保持当前 agent，直到其到达 `expire_at`
- 到期后按优先级选择新 agent，并先完成连接探测；仅在新 agent 可连接时才切换
- 若新 agent 持续不可连接，当前 agent 继续工作（直到自身下线/不可用）
- 通过 `(agent_name, instance_id)` 绑定，避免同名 agent 重启混淆
- 超过 `udp-max-payload` 的包默认丢弃并记日志

已知问题与后续改进：

- [x] 建立 P2P 连接后，`relay` 数据面当前没有心跳包；长时间空闲时可能被 NAT 回收映射，导致 P2P 中断（已实现保活心跳）
- [x] UDP relay 数据面已支持 `obfs-v1` 帧编码（`obfs_seed + nonce` 混淆头部元数据），不再使用固定明文 `flow_id + len` 包头特征（保留 `obfs_seed=0` 的 legacy 兼容模式）
- [ ] 当前未支持“多 p2p 通道智能路由”能力（当前为单 p2p tunnel）

### 多通道智能路由方案草案（待实现）

背景澄清：

- 目标是缓解“单通道 UDP 限速/阻断”风险，提高可用性与稳定性
- 对 `rtun relay` 来说，仅增加本地/目标端口规则收益有限
- 需要在 `relay <-> agent` 之间维持多条并行 p2p tunnel，并在 tunnel 间切换

方案要点：

- `relay` 侧维护一个 p2p tunnel 池（而不是单 tunnel）
- 同一个 `relay` 控制会话内，多条 tunnel 共享同一张 flow map
- 同一 `flow_id` 在 agent 侧始终复用同一个目标 UDP socket（不因 tunnel 切换重建）
- 不引入 `relay_session_id`，作用域按“单 relay 控制会话”隔离
- tunnel 选路不固定，基于“通畅度”做动态分配
- 通畅度指标包括：心跳丢失率、RTT(EWMA)、连续发送失败、近期吞吐
- 使用 `flow` 级迁移：新 flow 选更优 tunnel，已有 flow 在当前 tunnel 退化时迁移
- 回包可走任意可用 tunnel，由 flow 选路策略决定
- UDP 乱序/丢包按协议本身特性处理，上层业务需容忍

通道生命周期与容量规则：

- 每条通道绑定过期时间 `expire_at`
- 到期时触发创建新的替换通道
- 只有替换通道创建成功后，旧通道才停止分配新 flow（进入排空）
- 若替换通道持续创建失败，旧通道保持可分配状态并继续工作
- 旧通道进入排空后，flow 数降为 `0` 时再关闭通道
- 通道池支持最小/最大值：`--p2p-min-channels`、`--p2p-max-channels`
- 当 `active_flows == 0` 时，仅维持最小通道数
- 当 `active_flows > 0` 时，扩容至最大通道数
- v1 先要求 `--p2p-min-channels >= 1`，暂不支持 `min=0` 的按需建链模式
- flow 需按固定周期重选通道；到期通道在重选过程中会逐步排空

可观测性与运维界面（TUI）：

- `relay` 增加终端界面模式（`--tui`），用于实时展示运行状态
- Agents 视图：`name`、`instance_id`、`signal addr`、`expire_at`、当前选中状态
- Tunnels 视图：`tunnel_id`、本地/远端地址、状态、RTT、带宽、flow 数
- Flows 视图：`flow_id`、源地址、目标地址、所属通道、最近活跃时间、收发统计
- `--tui` 模式下不输出控制台滚动日志，避免界面被污染
- 仅当显式提供 `--log-file <path>` 时写日志文件；未提供时不落盘日志
- 非 `--tui` 模式保持现有控制台日志行为

### 实施里程碑（建议）

- P0：多通道智能路由核心能力（通道池、选路、flow 迁移、通道到期替换）
- P1：可观测性与运维（`--tui` + 关键事件日志）
- P2：自动化集成测试（覆盖端到端与切换场景，防回归）
- P3：稳定性与性能收敛（长跑测试、参数调优、热点路径优化）

### P2 自动化集成测试清单（防回归）

测试框架目标：

- 启动三个实例：`agent listen`、`agent pub`、`relay`
- 启动本地 UDP echo/业务模拟服务，验证端到端数据正确性

覆盖范围（包含但不限于）：

- 基础转发：`relay -L` 单规则与多规则
- 端到端正确性：双向收发、内容一致性、并发 flow
- 信令协议：`https://` 与 `quic://`
- agent 选择：按 `expire_at` 降序选择最新可用实例
- agent 切换：到期后“先连后切”；新 agent 不可连时保持旧 agent 工作
- 通道切换：到期替换、停止新 flow 分配、flow 排空后关闭旧通道
- flow 迁移：通道退化时迁移至更优通道（flow 级迁移）
- UDP 空闲超时：`--udp-idle-timeout` 行为正确
- UDP 负载上限：`--udp-max-payload` 超限丢包行为正确
- 编码兼容：`obfs-v1` 与 `obfs_seed=0` 兼容路径

当前已落地（最小框架）：

- 新增进程级集成测试：`rtun/tests/relay_e2e.rs`
- 默认用例：`relay_quic_smoke_e2e`
  - 启动 `agent listen`、`agent pub`、`relay`
  - 使用本地 UDP echo 服务验证端到端转发正确性
  - 信令路径：`quic://`
- 慢速用例（默认忽略）：`relay_quic_agent_switch_e2e`
  - 覆盖基于 `expire_in` 的 agent 切换连续性（`expire_in` 当前是分钟粒度）

运行方式：

```bash
# 仅跑默认 smoke
cargo test -p rtun --test relay_e2e relay_quic_smoke_e2e -- --nocapture

# 包含慢速 agent 切换用例
cargo test -p rtun --test relay_e2e -- --ignored --nocapture
```

说明：

- `https://` 信令的自动化 e2e 仍待补充（当前客户端没有 `https insecure` 测试开关，默认需受信证书链）

## Manual Release Workflow

GitHub Actions: `.github/workflows/build-manual-release.yml`

- 手动触发（`workflow_dispatch`）
- 编译 3 个目标：
  - `x86_64-unknown-linux-musl`
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
- 发布两个 Release：
  - 版本化：`rtun_YYMMDD_<commit7>`
  - 固定最新：`rtun_latest`

## Nightly Auto Build Workflow

GitHub Actions: `.github/workflows/build-nightly.yml`

- 推送到 `main` 分支时自动触发编译（`push`）
- 也支持手动触发（`workflow_dispatch`）
- 编译 3 个目标：
  - `x86_64-unknown-linux-musl`
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
- 发布到固定的 `nightly` 预发布（`tag_name: nightly`）
- 没有版本化 release id；每次执行都会更新同一个 `nightly`（覆盖同名资产）
- 与手动发布的 `rtun_latest` 不同，`nightly` 表示 `main` 分支的滚动最新构建
- 当前 `build-nightly.yml` 不生成 `.sha256` 文件

## Verify SHA256

下载二进制后，同时下载对应的 `.sha256` 文件并校验。

Linux:
```bash
sha256sum -c rtun-x86_64-unknown-linux-musl.sha256
sha256sum -c rtun-x86_64-apple-darwin.sha256
sha256sum -c rtun-aarch64-apple-darwin.sha256
```

macOS:
```bash
shasum -a 256 -c rtun-x86_64-unknown-linux-musl.sha256
shasum -a 256 -c rtun-x86_64-apple-darwin.sha256
shasum -a 256 -c rtun-aarch64-apple-darwin.sha256
```
