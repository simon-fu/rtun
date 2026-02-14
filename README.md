# rtun

## Build musl target
```bash
sudo yum install gcc
rustup target add x86_64-unknown-linux-musl
cargo build --release --target=x86_64-unknown-linux-musl
```

### Usage
```bash
# 服务
rtun agent listen --addr 0.0.0.0:8888 --https-key /certs/xxx.com.key --https-cert /certs/xxx.com.pem --secret sec123

# 出口方
rtun agent pub "https://xxx.com:8888" --agent rtun --expire_in 60 --secret sec123

# 入口方
rtun socks --listen 0.0.0.0:2080 "https://xxx.com:8888" --secret sec123
```bash
