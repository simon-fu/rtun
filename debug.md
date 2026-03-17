

## 调试
无论是哪个环境，涉及的配置文件，不要查看文件内容，如需要了解配置文件的信息，停下来问用户。  
shell 子命令的 agent 名字是文本匹配，比如 "nightly-rtun1-manual"。  
relay 子命令的 agent 名字是正则匹配，比如 '^nightly-rtun1-manual$' 。
不要在不同环境拷贝二进制运行文件，要用源码编译。  
调试 hard-nat 时，relay 侧用参数 --p2p-hardnat force， --p2p-hardnat-role nat4 。


### 远端环境

1. 配置文件 ~/simon/bin/config/rtun-full.toml，不要查看此文件，里面有敏感信息

2. PATH 路径已经有一个可用 rtun， 也可以用源码编译出来的 rtun 

3. 启动远端 交互式 shell，要指定 tty 参数，用 expect/python 分配 TTY
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

2. 无法访问github，源码要通过 scp 从本地复制过去

3. 配置文件 /tmp/rtun-work/rtun-full.toml ， 不要查看此文件，里面有敏感信息

4. 在 /tmp 工作，不要查看其他目录文件

5. 可以用 tcpdump

6. rtun-full.toml 里已经配置了参数 -L 0.0.0.0:14433 -> 127.0.0.1:4433
