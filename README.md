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

## Dev Auto Build Workflow

GitHub Actions: `.github/workflows/build-dev.yml`

- 推送到 `dev` 分支时自动触发编译（`push`）
- 也支持手动触发（`workflow_dispatch`）
- 编译 3 个目标：
  - `x86_64-unknown-linux-musl`
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
- 发布到固定的 `nightly` 预发布（`tag_name: nightly`）
- 没有版本化 release id；每次执行都会更新同一个 `nightly`（覆盖同名资产）
- 当前 `build-dev.yml` 不生成 `.sha256` 文件

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
