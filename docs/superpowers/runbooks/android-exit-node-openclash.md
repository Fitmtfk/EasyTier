# Android 全流量退出节点 + OpenClash 出口 Runbook

适用：Android EasyTier 客户端（stock GUI + 本仓库 `releases/mine` 自定义 VPN DNS 补丁）
将全部流量（含 DNS）经退出节点交给退出侧 OpenWrt VM（OpenClash）做统一出口分流。

设计背景见 `docs/superpowers/specs/2026-09-02-android-exit-node-custom-vpn-dns-design.md`。

占位符（全文替换后再执行）：

- `<EASYTIER_NET>`：easytier 虚拟网段，例如 `10.144.0.0/16`
- `<EXIT_IF>`：exit host 的上联（物理出网）网卡名，例如 `eth0`
- `<VM_IP>`：OpenWrt VM 对 exit host 可见的 LAN IP，例如 `192.168.122.2`

## 1. 拓扑（文字版）

```
Android 手机
  VPN 接口：0.0.0.0/0 全流量，DNS = <VM_IP>
      │  （easytier 隧道，加密）
      ▼
exit host（Linux，勾选 enable_exit_node 的 easytier 节点）
      │  内核转发 + 策略路由：来自 <EASYTIER_NET> 的流量 → table 100
      ▼
OpenWrt VM（OpenClash）
      │  fake-ip + 透明代理，Clash 规则分流
      ▼
Clash 规则出口（最终公网出口 IP = Clash 命中节点）
```

要点：host 自身默认路由不动，只有源自 `<EASYTIER_NET>` 的 VPN 流量被引到 VM，
因此 VM 的回程/上游流量不会再次进入 table 100，天然防环。

## 2. exit host：内核转发与 NAT

```bash
# 开启 IPv4 转发
sudo sysctl -w net.ipv4.ip_forward=1

# 持久化：写入 /etc/sysctl.conf（或 /etc/sysctl.d/99-easytier.conf）
echo 'net.ipv4.ip_forward = 1' | sudo tee /etc/sysctl.d/99-easytier.conf

# FORWARD 链放行 easytier 网段（若默认策略为 DROP；已有放行规则可跳过）
sudo iptables -A FORWARD -s <EASYTIER_NET> -j ACCEPT
sudo iptables -A FORWARD -d <EASYTIER_NET> -m state --state RELATED,ESTABLISHED -j ACCEPT

# SNAT：VPN 流量经上联网卡出网时伪装成 host 地址
sudo iptables -t nat -A POSTROUTING -s <EASYTIER_NET> -o <EXIT_IF> -j MASQUERADE
```

注意：easytier 的 TUN 流量转发本身由 core 完成（`enable_exit_node`），
上面三步是 OS 层把转发后的流量真正送出网络所需的条件。

## 3. 策略路由：仅 VPN 流量进 VM

若 VM 本来就是 host 的默认网关，可跳过本节（流量天然覆盖）。否则用策略路由，
只把来自 easytier 网段的流量交给 VM，host 自身默认路由不变：

```bash
sudo ip rule add from <EASYTIER_NET> table 100
sudo ip route add default via <VM_IP> table 100
```

重启持久化方式因发行版而异（不展开，两种常见提示）：

- systemd-networkd：在 exit host 侧网络单元里配置 `[RoutingPolicyRule]`
  `From=<EASYTIER_NET>` + `Table=100`，配合 `[Route]` `Gateway=<VM_IP> Table=100`。
- 简单脚本：把上面两条命令放入开机自启脚本
  （如 systemd `oneshot` unit 或网络 if-up 钩子）。

## 4. OpenClash 侧（OpenWrt VM）

- 在 OpenClash「DNS 设置」中开启 DNS 并监听 LAN 53：启用「DNS 转发 / 启用 DNS」，
  勾选「允许 LAN 主机访问 / 监听 0.0.0.0」类选项，使 `53/<VM_IP>` 对 LAN（即
  exit host 方向）可解析。
- fake-ip 模式下，从 exit host 转发进来的 VPN 流量进入 OpenWrt 后由
  PREROUTING 透明代理链接管，OpenClash 默认规则即可分流；
  **exit host 上无需对 53 端口做任何 DNAT**。
- 手机把 DNS 指向 `<VM_IP>` 即可：查询经隧道 → exit host 转发 → OpenWrt
  OpenClash 解析器（走 Clash 规则出口），不再泄漏到手机本地网络。

## 5. 手机侧配置（stock GUI + 本补丁）

1. easytier 网络配置 → 退出节点：`exit_nodes` 填 exit 节点的虚拟 IP。
2. 高级设置 → 启用手动路由，添加路由：`0.0.0.0/0`。
3. exit 节点端（同 GUI）：勾选 `enable_exit_node`。
4. 底部菜单 → 「自定义 VPN DNS」：填 `<VM_IP>`，保存后立即生效（VPN 自动重启）。

## 6. 验证

在 exit host 上直测 VM 解析器：

```bash
dig @<VM_IP> baidu.com
```

手机浏览器访问 `https://ip.sb`，显示出口 IP 应为 Clash 面板命中记录的出口 IP，
而不是手机本地公网 IP；同时在 Clash 面板连接列表中能看到对应连接。

## 7. 故障排查

- **首次查询很慢/超时**：先确认 exit host 的默认网关没有被改成指向 VM——
  host 自身 DNS 查询若被绕到 VM 再回不来，会表现为普遍变慢。
  `ip route show default` 应仍指向真实上游。
- **DNS 通但流量不出**：依次检查第 2 节三项：
  `sysctl net.ipv4.ip_forward`（应为 1）、FORWARD 放行规则、
  `iptables -t nat -L POSTROUTING` 中 `<EASYTIER_NET>` 的 MASQUERADE。
- **观察 DNS 查询去向**：

  ```bash
  sudo tcpdump -i any port 53 -n
  ```

  应看到查询从 easytier TUN 进来、从 host→VM 网卡出去；若出现在
  `<EXIT_IF>` 上发往公网 53，说明策略路由或手机 DNS 配置未生效。
- **fake-ip 解析出的 IP 连不上**：说明 OpenWrt 透明代理未接管转发流量。
  在 OpenClash 开启「透明代理（TProxy/Redirect）」，或将 Clash 改回 real-ip 模式。

## 8. 附录（本期不做）：`.et.net` 经 mihomo 解析

magic DNS 仅暴露 RPC 接口（`127.0.0.1:49813`），不是纯 UDP 53 服务，
mihomo/OpenClash 无法直接把它配置为 nameserver。候选路径：easytier core 的
`[dns_server]` 静态 zone 支持 UDP 53 监听，能否注入对端 `.et.net` 名单需单独验证。
本期不实现，仅记录。

## 补丁维护流程

本分支改动仅落在 `easytier-gui/` 下 3 个文件（`config.ts`、`mobile_vpn.ts`、
`index.vue`）+ 本文档，不触碰 Rust/proto，rebase 冲突面最小。

```powershell
# remotes（一次性）
git remote add origin   https://github.com/Fitmtfk/EasyTier   # fork
git remote add upstream https://github.com/EasyTier/EasyTier  # 上游

# 跟进上游
git fetch upstream
git rebase upstream/main          # 在 releases/mine 上执行

# 归档补丁（仓库同级目录）
git format-patch main..releases/mine -o ..\easytier-patches

# 新克隆恢复
git checkout -b mine
git am ..\easytier-patches\*.patch
```

push `releases/mine` 后 fork 的 `.github/workflows/mobile.yml` 会自动构建
4 ABI release APK；若产物签名不可安装，退回本地构建签名 APK
（`corepack pnpm --dir easytier-gui tauri android build --apk`）。
