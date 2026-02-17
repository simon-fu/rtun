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

## Relay Subcommand (Planned)

以下为 `relay` 子命令的需求定稿（计划中，尚未实现）。

- 子命令：`rtun relay`
- 规则参数：`-L [proto://]<listen_addr>?to=<target_addr>`
- `proto` 省略时默认 `udp`
- `-L` 可重复，用于配置多条转发规则
- 当前仅支持 `udp`，`tcp` 语法预留给后续扩展
- `target_addr` 当前仅支持 `IP:PORT`（不支持域名）
- 仅支持正向模式：本地监听 -> 远端目标 -> 回包返回本地发送方
- UDP 业务流量通过底层 P2P 通道转发，不使用现有 QUIC 隧道
- 支持参数：
  - `--udp-idle-timeout`（默认 `120s`）
  - `--udp-max-payload`（默认按底层通道可发送上限自动计算）
- 超过 `udp-max-payload` 的 UDP 包默认丢弃（并记录日志）

示例：

```bash
rtun relay \
  -L udp://0.0.0.0:15353?to=8.8.8.8:53 \
  -L 0.0.0.0:15354?to=8.8.4.4:53 \
  "quic://127.0.0.1:8888" \
  --secret sec123 --quic-insecure
```

### Relay Implementation Plan (Draft)

以下是 `relay` 的实现方案草案（仅方案，尚未编码）。

- 开发约束：
  - 尽量不改动原有代码路径，优先通过新增模块实现，降低回归风险
  - 若确实需要修改原有代码，必须先与需求方确认并获得同意后再改
- 总体原则：
  - `relay` 只做正向 UDP relay
  - 信令继续走现有 `https://` 或 `quic://`
  - UDP 业务数据通过底层 P2P 通道转发，不复用现有 socks 的 QUIC 隧道转发逻辑
  - agent 切换策略采用故障切换：选定后保持使用，当前 agent 下线/不可用时再重选
- 命令与参数：
  - `rtun relay -L <rule> [-L <rule> ...] <signal_url>`
  - 规则格式：`-L [proto://]<listen_addr>?to=<target_addr>`
  - `proto` 省略时默认 `udp`
  - 当前仅支持 `udp`；`tcp` 语法保留给后续扩展
  - 支持 `--udp-idle-timeout`（默认 `120s`）
  - 支持 `--udp-max-payload`（可选；默认按底层通道可发送上限自动计算）
- 数据面协议：
  - 每条 `-L` 规则启动一个独立 relay worker
  - worker 维护 `src_addr <-> flow_id` 会话映射
  - P2P datagram 帧携带 `flow_id + payload_len + payload`
  - 首包创建 flow，回包按 `flow_id` 反查并回送源地址
  - 保持 UDP 语义，不做可靠重传与重排
- 包长与丢弃策略：
  - 默认可发送上限：`底层 max_datagram_size - relay_header_len`
  - 若配置 `--udp-max-payload`，则使用用户值
  - 用户值超过底层上限时，启动报错并退出
  - 超过最终上限的包直接丢弃并记录 `warn` 日志
- 会话超时与清理：
  - 入口/出口两端均按 `--udp-idle-timeout` 回收空闲 flow
  - 默认 `120s`，周期扫描清理
  - 底层连接断开时清理映射，重连后自动重建
- agent 选择与切换：
  - agent 会话增加 `instance_id`（由服务端在 `agent pub` 建连时生成）
  - `sessions` 返回 `(name, instance_id, expire_at)`，`relay` 以 `(name, instance_id)` 作为绑定键
  - 即使 `name` 相同，只要 `instance_id` 改变（例如 agent 重启），也视为新实例
  - 初次选择：按 `expire_at` 降序选择可用 agent（过期时间最晚优先）
  - 运行中不做“到期前主动切换”，避免引入额外复杂度和会话抖动
  - 当前绑定实例（`name + instance_id`）下线/不可用后，才重新按 `expire_at` 降序选择新的可用实例
  - `sub` 建议支持可选 `instance_id` 精确绑定；同名但实例不匹配时返回错误
- 代码改动范围（规划）：
  - 新增命令模块：`rtun/src/bin/rtun/cmd_relay/`
  - 在 `rtun/src/bin/rtun/main.rs` 注册 `relay`
  - 扩展协议：`rtun/proto/app.proto` 新增 relay 专用 P2P args
  - 入口侧新增 relay 专用 pool 与 worker（不改 socks 现有行为）
  - 出口侧在 `rtun/src/switch/agent/ctrl.rs` 增加 relay P2P 处理分支
- 测试计划：
  - `-L` 规则解析单测（IPv4/IPv6、默认 proto、非法输入）
  - relay 帧编解码单测
  - 本地联调：多 `-L`、空闲超时、断线重连、超限包丢弃日志

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
