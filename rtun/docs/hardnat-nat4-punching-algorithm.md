# HardNAT NAT4 打洞流程算法

本文档记录 `nat4 <-> nat3` 场景下的 NAT4 打洞流程，用于统一 issue、调试和后续实现中的算法描述。

## 角色与记号

- `nat3_public_addrs[]`
  nat3 创建单个 UDP socket 后，通过 STUN 获取到的公网地址列表，包含端口。
- `nat4_candidate_ips[]`
  nat4 创建一批 UDP socket 后，通过 STUN 获取到的公网地址列表，去重 IP 后得到的公网 IP 列表。
- `N1`
  nat4 握手阶段用于确认连接稳定的连续收发轮次阈值。
- `N2`
  nat4 候选者阶段用于确认最终连通的连续收发轮次阈值。
- `N3`
  nat3 对同一个 `nat4_ip` 的随机端口批次重试次数上限。
- `N4`
  nat4 对 `nat3_public_addrs[]` 外层轮转的总尝试次数上限。

## 打洞流程

1. 一端是 nat4，另一端是 nat3。
2. nat3 创建一个 UDP socket，通过 STUN 获取外网地址列表，记作 `nat3_public_addrs[]`。
3. nat4 创建一批 UDP socket，通过 STUN 获取外网地址列表，去重 IP，得到 IP 列表，记作 `nat4_candidate_ips[]`。
4. nat4 从 `nat3_public_addrs[]` 里取出下一个地址 `nat3_addr`。
5. nat4 以 `nat3_addr` 为目标地址，所有 socket 执行 TTL warm up 到指定 TTL 值，然后进入接收状态。
6. nat3 从 `nat4_candidate_ips[]` 里取出下一个 IP `nat4_ip`。
7. nat3 针对目标 `nat4_ip` 随机取一批端口，对所有端口发送探测包。
8. nat4 如果有 socket 收到探测包，则将该 socket 的 TTL 设置为正常值，回复 ack 响应，并进入握手阶段。
9. nat4 处在握手阶段的 socket 连续发送 `N1` 个包并收到 nat3 的响应后，则认为连接稳定，尝试抢占候选者。
10. nat4 抢到候选者的 socket 进入候选者状态，其他 socket 进入握手等待状态。
11. nat4 候选者状态 socket 连续发送 `N2` 个包并收到 nat3 的响应后，进入连接成功状态，其他 socket 释放资源，打洞成功结束。
12. nat4 候选者状态如果超时没有建立连接，则释放候选者，所有握手等待状态的 socket 重复步骤 9。
13. 如果步骤 8 没有 socket 收到探测包，超时后重复步骤 7。
14. 如果步骤 7 尝试 `N3` 次后仍未建立连接，则重复步骤 6。
15. 如果步骤 6 已经取完所有 IP，则重复步骤 4。
16. 如果步骤 4 已经取完所有地址，则从 `nat3_public_addrs[]` 第一个地址再次尝试。
17. 如果步骤 4 的外层轮转尝试达到 `N4` 次后仍未建立成功，则打洞失败结束。

## 说明

- nat3 使用单个 UDP socket 执行探测，并通过 `nat3_public_addrs[]` 向 nat4 暴露可尝试的公网地址。
- nat4 使用一批 UDP socket 执行 TTL warm up 和接收，只有命中的 socket 才进入握手和候选者竞争。
- probing 阶段使用较小 TTL，只有在步骤 8 命中后才恢复为正常 TTL，避免把低 TTL 带入后续数据面。
