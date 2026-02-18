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
- 运行中保持当前 agent；当前 agent 下线/不可用后再重选
- 通过 `(agent_name, instance_id)` 绑定，避免同名 agent 重启混淆
- 超过 `udp-max-payload` 的包默认丢弃并记日志

已知问题与后续改进：

- [x] 建立 P2P 连接后，`relay` 数据面当前没有心跳包；长时间空闲时可能被 NAT 回收映射，导致 P2P 中断（已实现保活心跳）
- [ ] 当前 UDP 数据包格式固定为 `flow_id(6 bytes) + len(2 bytes) + payload(len bytes)`，容易被运营商/GFW做特征识别并触发限速或断连
- [ ] 当前未支持 Hysteria2 的端口跳跃能力

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
