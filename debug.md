

## 调试
无论是哪个环境，涉及的配置文件，不要查看文件内容，如需要了解配置文件的信息，停下来问用户。  
shell 子命令的 agent 名字是文本匹配，比如 "nightly-rtun1-manual"。  
relay 子命令的 agent 名字是正则匹配，比如 '^nightly-rtun1-manual$' 。
不要在不同环境拷贝二进制运行文件，要用源码编译。  
调试 hard-nat 时，relay 侧用参数 --p2p-hardnat force， --p2p-hardnat-role nat4 。

### 固定初始化流程

调试前，先在当前 worktree 根目录执行下面两个脚本，统一把 `rtun-local` 和 `nightly` 拉回可调试基线：

1. 初始化 `rtun-local`
    ```shell
    python3 rtun/tests/manual/init_rtun_local.py
    ```
    - 同步“当前 worktree 内容”到 `rtun-local:/tmp/rtun-local-work`
    - 清理该目录下旧的调试 `rtun` 进程
    - 在 `rtun-local` 上重新编译 `rtun`

2. 初始化 `nightly`
    ```shell
    python3 rtun/tests/manual/init_nightly.py --branch issue/9-hardnat-protocol
    ```
    如需切换基础 shell agent 名：
    ```shell
    python3 rtun/tests/manual/init_nightly.py --branch issue/9-hardnat-protocol --shell-agent nightly-rtun1-manual
    ```
    - 通过 `rtun shell` 接入远端 shell
    - 清理 `/tmp/rtun-nightly-work`
    - 清理固定调试 tmux session `rtun-debug-agent`
    - 只清理由 `/tmp/rtun-nightly-work` 启动的旧调试 `rtun`
    - 用 Git 拉取指定分支并重新编译 `rtun`
    - 这里拉的是 `origin/<branch>` 的远端 tip，不是“当前本地 worktree 的未提交内容”
    - 如果后续要用本地源码解释 nightly 现象，先核对两边 commit 是否一致；不一致时，以 nightly 实际 commit 为准
    - 建议初始化后立刻记录：
      ```shell
      cd /tmp/rtun-nightly-work && git rev-parse HEAD
      ```

3. 这两个脚本只负责“环境初始化”，不自动启动新的调试 agent，也不自动启动 relay。

### manual NAT4 一键脚本

如果目标是“先把 manual NAT4 基线自动跑通”，优先使用下面这个总控脚本，而不是每次手工敲完整流程：

```shell
python3 rtun/tests/manual/run_manual_nat4_auto.py \
  --init-rtun-local \
  --init-nightly \
  --nightly-source worktree \
  --summary-json /tmp/manual-nat4-auto-summary.json
```

- 默认 `rtun-local host` 是 `rtun-local`
- 默认远端 shell agent 是 `nightly-rtun1-manual`
- 如需切换环境名字，统一通过参数传：
  ```shell
  python3 rtun/tests/manual/run_manual_nat4_auto.py \
    --rtun-local-host rtun-local \
    --shell-agent nightly-rtun1-manual
  ```
- 这个脚本会自动：
  - 可选初始化 `rtun-local` 和 nightly
  - 采样本轮 `nat4_candidate_ips`
  - 如果本轮包含 `101.40.161.229`，优先先打它；否则按采样顺序遍历
  - 自动解析 `nat3_public_addr`
  - 自动推进固定批次数 probing
  - 任一 IP 满足成功条件后自动停止
- 当前成功条件固定为：
  - nat3 日志出现 `connected from target [...]`
  - nat4 日志出现 `manual converge final connected selected: owner [...]`
- 如果不加 `--init-rtun-local` 或 `--init-nightly`
  - 脚本会复用现有环境
  - 但这不保证当前二进制和代码版本是最新；脚本会打印 warning，调用方仍要对环境状态负责
- `--summary-json` 只负责输出本轮每个 IP 的摘要，不替代 issue 记录


### 远端环境

1. 配置文件 ~/simon/bin/config/rtun-full.toml，不要查看此文件，里面有敏感信息

2. PATH 路径已经有一个可用 rtun， 也可以用源码编译出来的 rtun 

3. 手工启动远端交互式 shell 时，要指定 tty 参数；平时优先用 `rtun/tests/manual/init_nightly.py`
    ```shell
    bash -lc stty rows 24 cols 80; rtun --config ~/simon/bin/config/rtun-full.toml shell --agent 'nightly-rtun1-manual'
    ```

4. 远端系统信息：
    - 是一个ubuntu系统
    - git已经登录，可以clone/pull本repo代码
    - cpu 核心少，性能比较弱，尽量增量编译
    - 网络是 cone NAT3
    - agent有过期时间，过了过期时间就重置整个环境（包括网络出口ip会变动）

5. 如果修改代码要传到远端，用 feature 分支传，不要用 dev/main 分支传

6. 远端已经启动了agent，不要停止此agent。调试新 agent 名不能和已有的相同，agent 参数可以从环境变量获取，
    ```shell
    # 起新的agent例子
    path/to/rtun agent pub $MY_RTUN1_URL --agent new-agent --secret $MY_RTUN1_SECRET
    ```
7. rtun-full.toml 里已经配置了参数 -L 0.0.0.0:14433 -> 127.0.0.1:4433

8. 远端在 /tmp 工作，不要查看其他目录文件

### 本地环境

1. 本地调试，使用源码编译后的rtun
    ```shell
    path/to/rtun --config ~/simon/bin/config/rtun-full.toml relay
    ```

### rtun-local 环境

1. 登录方式：ssh rtun-local 
   系统：ubuntu
   rust： 已安装
   网络： nat4

2. 无法访问github，源码优先通过 `rtun/tests/manual/init_rtun_local.py` 从本地同步过去

3. 配置文件 /tmp/rtun-work/rtun-full.toml ， 不要查看此文件，里面有敏感信息

4. 在 /tmp 工作，不要查看其他目录文件

5. 可以用 tcpdump

6. rtun-full.toml 里已经配置了参数 -L 0.0.0.0:14433 -> 127.0.0.1:4433

### manual NAT4 fresh session 固定流程

下面这套流程用于“只验证 manual NAT4 基线，不改代码”。

1. 先重新采样 `rtun-local` 的当前 `nat4_candidate_ips`
    ```shell
    ssh rtun-local 'cd /tmp/rtun-local-work && timeout 18s env RUST_LOG=debug stdbuf -oL -eL target/debug/rtun nat4 nat4 -t 1.1.1.1:9 -c 128 --dump-public-addrs 2>&1 | python3 -c '\''import re,sys; text=sys.stdin.read(); m=re.search(r"candidate_ips \\[(.*?)\\]", text, re.S); print(m.group(1) if m else "")'\'''
    ```
    - 现在 `--dump-public-addrs` 走的是 dedicated sampler，主日志要看 `candidate_ips [...]`，不是旧的每 socket `mapped [...]`
    - 输出顺序就是当前 sampler 给出的优先级顺序
    - 当前 fresh session 可能同时出现 `60.194.*` 和 `101.*` 多个 IP，不要预设固定主次顺序
    - 不要直接复用上一次 session 的 IP 结论，每一轮都先重新采样

2. 清理两边旧 session 和旧日志
    - nightly：
      - `tmux kill-session -t rtun-debug-agent 2>/dev/null || true`
      - `tmux kill-session -t rtun-tcpdump 2>/dev/null || true`
      - 只杀 `/tmp/rtun-nightly-work` 启动的旧 `rtun`
      - `rm -f /tmp/rtun-nightly-work/nat3-manual.log /tmp/rtun-nightly-work/nat3-tcpdump.log`
    - rtun-local：
      - `tmux kill-session -t rtun-manual-nat4 2>/dev/null || true`
      - `tmux kill-session -t rtun-manual-tcpdump 2>/dev/null || true`
      - 只杀 `/tmp/rtun-local-work` 启动的旧 `rtun`
      - `rm -f /tmp/rtun-local-work/nat4-manual.log /tmp/rtun-local-work/nat4-tcpdump.log`

3. 在 nightly 启动 nat3
    ```shell
    cd /tmp/rtun-nightly-work
    tmux new-session -d -s rtun-debug-agent \
      "timeout 180s env RUST_LOG=debug stdbuf -oL -eL target/debug/rtun nat4 nat3 -t 101.39.212.231 -c 512 --interval 500 --discover-public-addr --pause-after-discovery --hold-batch-until-enter --debug-converge-lease"
    tmux pipe-pane -o -t rtun-debug-agent 'cat >> /tmp/rtun-nightly-work/nat3-manual.log'
    ```
    - `-t` 先用主 IP；如果连续多批没有命中，再切到下一个 `nat4_candidate_ip`
    - `pipe-pane` 只是旁路记日志，nat3 仍然必须直接挂在 tmux pane 上

4. 如需在 nightly 补“探测包是否真的发出”的旁证，单独再起 `sudo tcpdump`
    ```shell
    cd /tmp/rtun-nightly-work
    tmux new-session -d -s rtun-tcpdump \
      "timeout 18s sudo tcpdump -Q out -nn -i any host 101.39.212.231 and udp > /tmp/rtun-nightly-work/nat3-tcpdump.log 2>&1"
    ```
    - `host` 换成当前正在测试的 `nat4_candidate_ip`
    - 这个抓包不是主证据链，只是旁证
    - 如果 `sudo tcpdump` 也失败，要把失败信息一起记录下来

5. 从 tmux pane 读取 nat3 本轮 discovery 结果
    ```shell
    tmux capture-pane -pJ -t rtun-debug-agent -S -120 | tail -n 40
    ```
    关注：
    - `nat3 public address discovery local [...] => mapped [...]`
    - `recommended peer command: rtun nat4 nat4 -t <nat3_public_addr>`
    - `nat3 discovery finished, press Enter to start probing`

6. 在 rtun-local 启动 nat4
    ```shell
    ssh rtun-local 'cd /tmp/rtun-local-work && tmux new-session -d -s rtun-manual-nat4 "timeout 180s env RUST_LOG=debug stdbuf -oL -eL target/debug/rtun nat4 nat4 -t <nat3_public_addr> -c 512 --interval 500 --debug-keep-recv --debug-promote-hit-ttl 64 --debug-converge-lease > /tmp/rtun-local-work/nat4-manual.log 2>&1"'
    ```
    - `--debug-converge-lease` 和 `--debug-keep-recv` 必须一起带
    - 当前手工调试固定再带 `--debug-promote-hit-ttl 64`

7. 如需确认本地是否真的收到来自 nightly 的探测包，在 rtun-local 再起一条 tcpdump
    ```shell
    ssh rtun-local 'cd /tmp/rtun-local-work && tmux new-session -d -s rtun-manual-tcpdump "timeout 25s tcpdump -l -nn -ni any host <nat3_public_ip> and udp > /tmp/rtun-local-work/nat4-tcpdump.log 2>&1"'
    ```
    - 先用 `host <nat3_public_ip> and udp`，同时观察：
      - 本地 nat4 发往 nat3 discovery 地址的出站 token
      - nat3 打回来的入站探测/回包
    - 统计时要区分方向：
      - `Out IP ... > <nat3_public_ip>.<port>` 视为本地出站
      - `<nat3_public_ip>.<port> > ...` 视为 nat3 入站

8. 回到 nightly，手工推进 probing
    ```shell
    tmux send-keys -t rtun-debug-agent C-m
    ```
    - 每按一次 `Enter`，nat3 会 reroll 一批随机目标端口
    - 一般先打 3 到 5 批；如果完全没有命中，再切换到下一个 `nat4_candidate_ip`
    - 不允许只测主 IP 就停下来给结果；当前 fresh session 采样出来的 `nat4_candidate_ips` 必须全部跑完，至少把每个 IP 都完成一轮固定批次数 probing，再统一总结结果

9. 检查四份关键证据
    - nightly：
      - `/tmp/rtun-nightly-work/nat3-manual.log`
      - `/tmp/rtun-nightly-work/nat3-tcpdump.log`
    - rtun-local：
      - `/tmp/rtun-local-work/nat4-manual.log`
      - `/tmp/rtun-local-work/nat4-tcpdump.log`
    - 关键日志：
      - nat3：`nat hello`
      - nat3：`connected from target`
      - nat4：`manual converge recv`
      - nat4：`promoted recv socket ttl`
      - nat4：`connected from target`
      - nat4：`candidate ready`
      - nat4：`lease granted`
      - nat4：`owner enter validating`
      - nat4：`final connected selected`

### 每轮测试结果最少记录项

每次测试结束后，至少要记录下面 5 项，不满足这 5 项就不算一轮完整记录。

另外，当前 fresh session 采样出来的 `nat4_candidate_ips` 必须全部覆盖完，才能停止测试并给出本轮结论。

测试结果不要继续沉淀到 `debug.md`。
- 每轮结果统一写到对应 GitHub issue
- issue 里的结果必须使用折叠块
- `debug.md` 只保留：
  - 固定流程
  - 参数约束
  - 已验证坑点
  - 结果记录模板要求

1. nat4 的外网 IP 列表，以及本轮使用多少个 socket
    - 外网 IP 列表来自 `--dump-public-addrs` 采样结果
    - socket 数量来自 nat4 启动参数 `-c`

2. nat3 的外网地址列表
    - 来自 nat3 discovery 日志
    - 记录 `nat3 public address discovery ... => mapped [...]`
    - 如果后续扩展成多个地址，这里要把所有地址都列出来

3. nat3 针对某个 nat4 IP 一共换了多少批随机端口
    - 直接记录本轮人工按了多少次 `Enter`
    - 如果切换了 `nat4_candidate_ip`，每个 IP 的批次数要分别记

4. nat4 有多少个 socket 收到探测包，是否升级了 TTL
    - 用 `nat4-manual.log` 统计
    - 先看有没有 `manual converge recv ...`
    - 再看有没有 `promoted recv socket ttl [...]`
    - 要给出：
      - 收到探测包的 socket 数
      - 是否发生 TTL 升级

5. nat3 是否收到 nat4 的回包
    - 优先看 nat3 日志里有没有：
      - `connected from target`
    - 如果没有，就明确记为“未收到回包”

### 已验证坑点

1. nat3 不能用 `> file 2>&1` 方式启动
    - 这样会把 `pause-after-discovery` / `hold-batch-until-enter` 的交互链路搞坏
    - 正确做法是：nat3 直接挂在 tmux pane，上面再叠 `tmux pipe-pane`

2. `--debug-converge-lease` 不能单独在 nat4 上开
    - 当前本地手工测试必须同时加 `--debug-keep-recv`

3. 每轮 fresh session 都要重新采样 `nat4_candidate_ips`
    - 当前可能采到 `60.194.*`、`101.*` 多个 IP
    - 不要把某个 IP 当成永久固定主 IP

4. 如果 primary IP 连续多批完全没有 `manual converge recv`
    - 不要在同一个 IP 上无限重试
    - 按算法切到下一个 `nat4_candidate_ip`

5. `tmux ls` 显示 `no server running` 时，不要继续 `send-keys`
    - 说明 nat3 session 已经退出，或者远端 shell agent 这一侧的 tmux server 已经重置
    - 这时要重新起 `rtun-debug-agent`，不要对着不存在的 session 继续操作

6. nightly 上 `sudo tcpdump` 仍然只能当旁证
    - 当前已经确认 `sudo -n` 可用
    - 但 `sudo tcpdump -Q out -nn -i any host <nat4_ip> and udp` 这一条，在有 root 的情况下仍可能出现：
      - `0 packets captured`
      - `0 packets received by filter`
    - 所以远端优先依赖：
      - nat3 自身日志
      - 本地 rtun-local 的 tcpdump

7. 不要默认 nightly 和本地 worktree 是同一版代码
    - `init_nightly.py` 会把 nightly 重置到远端 `origin/<branch>` 的当前 tip
    - 本地 worktree 如果有未推送提交，或者远端分支 tip 比本地旧/新，两个环境看到的 `hard_nat.rs` 都可能不是同一版
    - 出现“本地代码逻辑解释不了 nightly 现象”时，先检查：
      - 本地：`git rev-parse HEAD`
      - nightly：`cd /tmp/rtun-nightly-work && git rev-parse HEAD`
    - 如果 commit 不一致，不要继续拿本地实现推理 nightly；先按 nightly 实际 commit 复现和定位

8. `init_nightly.py` 当前偶发会出现“脚本返回 build 失败，但远端 cargo 最后继续收尾并产出二进制”
    - 如果脚本报 `cargo build -p rtun --bin rtun` 失败，不要立刻判定 nightly 初始化失败
    - 先确认两件事：
      ```shell
      cd /tmp/rtun-nightly-work && ls -l target/debug/rtun
      ps -ef | grep "cargo build -p rtun --bin rtun" | grep -v grep
      ```
    - 如果二进制已经生成，或者 cargo 还在跑，先以远端实际状态为准，再决定要不要重置环境

9. rtun-local 的 tcpdump 如果是 0 包，不是“没抓到日志”这么简单
    - 这通常表示：nightly 的探测包没有真正到达 rtun-local 主机
    - 这条证据要和 nat3 的持续 `nat hello`、nat4 的持续 `send token` 放在一起看

10. 如果某一轮出现了阶段性进展
    - 不要把“某个 IP 在某轮命中过”这种结果沉淀到 `debug.md`
    - 结果写 issue 折叠块
    - `debug.md` 只沉淀：
      - 下次仍然要复用的流程
      - 下次仍然要注意的坑
      - 下次仍然要采集的观测项

11. 解析 nat3 discovery 输出时，不要直接用普通 `tmux capture-pane`
    - 如果 pane 宽度不够，`recommended peer command` 这一行会被折行，端口号可能被截断
    - 固定使用 `tmux capture-pane -pJ -t rtun-debug-agent ...`
    - `-pJ` 会把折行重新拼回一行，便于准确抠出 `<nat3_public_addr>`
