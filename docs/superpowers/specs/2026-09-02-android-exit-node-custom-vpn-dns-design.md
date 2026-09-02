# Android 全流量退出节点 + 自定义 VPN DNS 设计

日期：2026-09-02
分支：`releases/mine`（fork: `Fitmtfk/EasyTier`，upstream: `EasyTier/EasyTier`）
基线：upstream main @ 164e2db

## 1. 目标与背景

让 Android 客户端（VPN 实现）把所有流量（含 DNS）代理到指定的 EasyTier 退出节点，并由退出侧的
OpenClash 统一处理出口分流；同时建立可持续的补丁维护流程，使改动能在未来上游版本上低成本重放。

已验证的现状与限制：

- 退出节点转发在核心中已存在（`--exit-nodes` / `--enable-exit-node`，
  `easytier-core/src/peers/peer_manager.rs` IPv4/IPv6 转发），GUI 高级设置已暴露
  `exit_nodes`、`enable_exit_node`、手动路由（`enable_manual_routes` + `routes`）。
- 手动路由严格 IPv4（`cidr::Ipv4Cidr` 解析），IPv6 全流量本期不做。
- Android VPN 的 DNS 在 `easytier-gui/src/composables/mobile_vpn.ts:363` 硬编码为
  `enable_magic_dns ? '100.100.100.101' : undefined`。magic DNS 的转发上游在 Android 上
  落到硬编码公共 DNS（223.5.5.5 / 180.184.1.1，`easytier/src/common/dns.rs`），查询从
  VPN 排除进程直接走手机本地网络 → 慢（首查超时重试）、泄漏、不经过退出节点。
- magic DNS 对外只暴露 RPC（`tcp://127.0.0.1:49813`），不是普通 UDP DNS 端口。

## 2. 补丁维护机制

- 本地分支 `releases/mine` 承载全部改动；`main` 永远与 upstream 同步，不承载本地提交。
- remotes：`origin` = fork（Fitmtfk/EasyTier），`upstream` = EasyTier/EasyTier
  （需执行 `git remote add upstream https://github.com/EasyTier/EasyTier`）。
- CI：fork 的 `.github/workflows/mobile.yml` 在 push 到 `releases/**` 时自动构建 4 ABI
  release APK（产物见 Actions upload-artifact）；分支名 `releases/mine` 天然命中触发器，
  不修改 workflow 文件。若 CI 产物 APK 签名不可安装，退回本地构建签名 APK。
- 跟进上游：`git fetch upstream && git rebase upstream/main`（在 `releases/mine` 上）→
  push fork → CI 出 APK。
- 归档保险：`git format-patch main..releases/mine -o patches/`；新环境
  `git checkout -b mine && git am patches/*.patch`。
- 原则：改动只落在 `easytier-gui/` 下 3 个文件，不触碰 Rust、proto、
  `easytier-web/frontend-lib` 等上游热区，最小化 rebase 冲突面。

## 3. Android 端改动（唯一代码改动，全部在 easytier-gui）

### 3.1 `src/composables/config.ts`

仿照现有 localStorage 助手，新增全局单值读写：

- `saveCustomVpnDns(dns: string | null)` / `loadCustomVpnDns(): string | null`
- key：`custom_vpn_dns`；空串/`null` = 不启用。
- 保存前校验为合法 IPv4（简单正则即可），非法输入 toast 报错不落盘。

### 3.2 `src/pages/index.vue`

底部设置菜单（`setting_menu_items`）新增一项"自定义 VPN DNS"，
`visible: () => type() === 'android'`；点击打开小 Dialog（仿照 config-server
Dialog 模式：InputText + Save/Cancel），保存时写 localStorage 并调用
`syncMobileVpnService()` 立即生效。标签文字直接用中文字面量，不动 locale 文件。

### 3.3 `src/composables/mobile_vpn.ts`

`reconcileNetworkInstance` 中（现 line 363）DNS 选择优先级改为：

```
custom_vpn_dns（非空且合法） > enable_magic_dns ? '100.100.100.101' : undefined
```

其余传播链复用现有逻辑：`dnsChanged` 比较（line 368）已会触发 VPN 重启，
`doStartVpn` 原样把 dns 传给 `start_vpn`。

### 3.4 数据流

设置对话框 → localStorage(`custom_vpn_dns`) → `syncMobileVpnService()` →
`reconcileNetworkInstance` 读取 → `doStartVpn(dns)` → VpnService `addDnsServer`。

约 60 行改动；不新增 core 配置字段，不改变 magic DNS 在无自定义值时的既有行为；
桌面端不受影响（入口与读取均在 Android 路径）。

## 4. 退出侧 runbook（零代码改动，运维文档）

拓扑：手机 VPN（`0.0.0.0/0`，DNS = OpenWrt VM LAN IP）→ 隧道 → exit host（Linux）→
转发至 OpenWrt VM（OpenClash）→ Clash 规则出口。

1. exit host：`net.ipv4.ip_forward=1`；FORWARD 放行 easytier TUN 网段；
   `iptables -t nat -A POSTROUTING -s <easytier网段> -o <出口网卡> -j MASQUERADE`。
2. 把 VPN 流量交给 OpenWrt VM：VM 已是 host 默认网关则天然覆盖；否则策略路由只影响
   VPN 流量：`ip rule add from <easytier网段> table 100` +
   `ip route add default via <VM_IP> table 100`。
3. 手机侧（stock GUI + 本补丁）：`exit_nodes` = exit vip；手动路由 `0.0.0.0/0`；
   自定义 VPN DNS = VM 的 LAN IP。
4. exit 节点侧 GUI：勾选 `enable_exit_node`。
5. OpenClash：开启 DNS 监听并允许 LAN 连接；fake-ip 模式的转发流量经过 PREROUTING
   透明代理链，OpenClash 默认规则可覆盖；host 上无需 53 端口 DNAT。
6. 附录（本期不做，仅记录）：`.et.net` 名称解析——magic DNS 仅暴露 RPC
   （`127.0.0.1:49813`），mihomo 无法直接使用；候选方案为 easytier `[dns_server]`
   zone 配置的 UDP 监听能否注入对端名单，未来单独验证。

## 5. 验收标准

- 手机访问 `ip.sb`：出口 IP = Clash 规则命中的出口 IP，而非手机本地 IP。
- 首查 DNS 无明显 5s 超时；host 上 `dig @<VM_IP> <域名>` 直测解析器可用。
- 自定义 DNS 留空时行为与上游一致（回退 magic DNS / 不设 DNS）。
- 桌面端与其他平台行为零变化（改动仅在 Android 路径）。
- push `releases/mine` → Actions 产物 APK 可安装可用。
- `git fetch upstream && git rebase upstream/main` 干跑通过（无冲突或冲突可解）。
- `easytier-gui` 现有单测（`pnpm test` / vitest，含 `mobile_vpn.test.ts`）通过；
  若单测覆盖 dns 计算则补充自定义 DNS 优先级用例。

## 6. 明确不做

- IPv6 全流量（手动路由 IPv4-only 是上游设计）。
- core/proto 层改动（socket protect、per-socket 路由、新配置字段）。
- `easytier-web/frontend-lib` 改动。
- magic DNS 上游可配置化。
