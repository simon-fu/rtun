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
