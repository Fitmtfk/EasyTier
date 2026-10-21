# 直连族偏好（prefer_ipv4 / prefer_ipv6）设计

日期：2026-10-21
状态：待评审（第二版，已按代码审阅意见修正）

## 1. 背景与目标

场景：电脑的 WiFi 只有 IPv4，通过手机蓝牙 PAN 接入 IPv6。电脑因此能加入
EasyTier 的 IPv6 虚拟网，但 PAN 带宽极低。期望是：**依然用 IPv4 打洞**，
打洞成功后把数据切到 IPv4 走，IPv6 连接保留作回退。

现状的问题：

- `Peer::select_conn` 的排序键只有延迟，没有 IP 族偏好
  （`easytier-core/src/peers/conn/peer.rs:30`）。
- 已有任意直连就不再生效新的打洞任务
  （`easytier-core/src/connectivity/direct/mod.rs:349`、`:413`、`:468`、`:599`、`:603`）。
- 对称 NAT 打洞在"已有任意连接"时直接跳过
  （`easytier-core/src/connectivity/hole_punch/peer_adapters.rs:59`、`:156`）。

目标：新增一个三值开关 `direct_conn_family`：

| 值 | 含义 |
| --- | --- |
| `auto`（默认） | 保持上游行为：先到先得 + 按延迟选 |
| `prefer_ipv4` | 优先用 IPv4 建立并使用直连；IPv6 保留作回退 |
| `prefer_ipv6` | 优先用 IPv6 建立并使用直连；IPv4 保留作回退 |

明确不做：

- 不关闭 IPv6 的监听、接入与中继能力。
- 不改变中继（relay）路径的选择逻辑，只改**直连**路径。
- 不改协议、不改线格式。

## 2. 范围

作用面共三处，全部限定在"同一个 peer 的多条直连"范围内：

1. **选路**：多条直连里选哪一条承载数据。
2. **发起**：已有一条非偏好族的直连时，是否还要继续尝试偏好族。
3. **对称 NAT 打洞**：已有非偏好族连接时，是否还要打洞。

不含：IPv6 监听开关、中继选择、路由表（OSPF）选路、协议层。

## 3. 现状

### 3.1 连接信息里有没有"族"

每条连接 `PeerConn` 带 `tunnel_info: Option<TunnelInfo>`
（`easytier-core/src/peers/conn/peer_conn.rs:289`），其中
`local_addr` / `remote_addr` / `resolved_remote_addr` 是 `common.Url`
（`easytier-proto/proto/common.proto:234`），由
`easytier-core/src/connectivity/protocol/raw.rs:478` 的 `connected_tunnel_info()` 填充。

判定族的三条现成路径：

- `easytier-proto/src/common.rs:405` `infer_tunnel_ipv6(raw) -> Option<bool>`
  —— 按 URL host 是 Ipv4/Ipv6/Domain 判定，Domain 返回 `None`。
- `easytier-proto/src/common.rs:467` `Url::is_ipv6_tunnel_endpoint()` —— 上面那个的
  布尔封装。
- `easytier-proto/src/common.rs:518` `TunnelInfo::effective_remote_addr()`
  —— `resolved_remote_addr.or(remote_addr)`，**优先用解析后的真实地址**。

两个坑：

- `PeerConn::tunnel_info` 是**私有字段**（`peer_conn.rs:289`），没有公开访问器。
- `PeerConn::get_conn_info()`（`peer_conn.rs:1474`）内部是
  `self.info.as_ref().unwrap()`（`:1475`），而 `info` 在握手完成前是 `None`
  → **未完成握手的连接调用它会 panic**。所以不能用它取族信息。

连接来源与 origin（`easytier-core/src/peers/mod.rs:32`）：

- `Manual | Direct | Listener | TcpHolePunch | UdpHolePunch | Attached`
- `is_hole_punched()`（`peer_conn.rs:441`）只对 `TcpHolePunch` / `UdpHolePunch` 为真。
- 关键：直连连接器打的 IPv4 洞（`connect_public_ipv4`，`direct/mod.rs:711`）
  走 `self.admit(...)`，origin 是 **`Direct`**（`direct/mod.rs:876`）→
  **直连打洞得到的连接不算 `is_hole_punched()`**。这决定了 §4.3 排序键第一分量
  不会压制它。
- `Peer::has_directly_connected_conn`（`peer.rs:289`）= 存在未关闭**且非
  hole_punched** 的连接 → 上面那种直连打洞连接**算**直连。

### 3.2 选路

`easytier-core/src/peers/conn/peer.rs`：

- `fn conn_latency_sort_key(latency_us: u64, is_hole_punched: bool) -> (bool, u64)`
  （`:30`）= `(is_hole_punched && latency_us == 0, latency_us)`。
  语义：刚打洞进来、首个 ping 还没确认（延迟为 0）的连接不抢流量。
- `select_conn`（`:214`）用 `min_by_key(conn_latency_sort_key)` 选默认连接，
  结果缓存在 `default_conn: Arc<ArcSwapOption<PeerConn>>`。
- 缓存失效：连接关闭时清（`:92`）；`default_conn_clear_task`（`:132`）每 5 秒
  检查一次，`conns.len() > 1` 时把缓存置空 → 多连接时最多 5 秒重新选一次。
- `send_msg`（`:240`）优先用缓存。

### 3.3 发起侧（直连连接器）

`easytier-core/src/connectivity/direct/mod.rs`：

- `DirectConnectorOptions`（`:122`）：`default_protocol`、`enable_ipv6`、
  `allow_public_server`、`bind_device`、`allow_interface_bind`、`tcp_bind`、
  `udp_bind`、`testing`；由 `easytier-core/src/instance/config.rs:398` 从 flags 构造
  （`enable_ipv6: flags.enable_ipv6`，`allow_public_server: true` 写死）。
  `socket_context(transport, ip_version)`（`:151`）可指定解析/连接的地址族。
- `DirectConnectorData`（`:163`）持有 `peer_manager`、`options`、
  `listener_blacklist`（URL 级，300s）、`peer_blacklist`（peer 级，只装
  `InvalidServiceKey`，3600s）。
- `collect_peers_need_task`（`:322`）过滤链里
  `!data.peer_manager.has_directly_connected_conn(route.peer_id)`（`:349`）。
- `try_direct_connect`（`:370`）：退避 `[1000,2000,2000,5000,5000,10000,30000,60000]`，
  循环体取对端 IP 列表 → `try_direct_connect_with_ip_list` → 成功后
  `if has_directly_connected_conn(dst_peer_id) { return Ok(()) }`（`:413`）→
  否则下一轮。**退避封顶 60s，循环永不结束。**
- `try_direct_connect_with_ip_list`（`:419`）：过滤 `ring` scheme（`:429`）、
  必须有 host+port（`:430`）、按 `enable_ipv6` 排除 IPv6（`:433`）；
  按 scheme 排序（default_protocol=3 > udp=2 > 其他=1，`:442`）后分组并发；
  每组结束检查 `has_directly_connected_conn`（`:468`）。
- `spawn_direct_connect_tasks`（`:475`）：按 listener host 形态分派 ——
  `0.0.0.0` → 展开 `interface_ipv4s` + `public_ipv4`（`:507`）；
  `[::]` → 展开 `interface_ipv6s` + `public_ipv6`（`:537`）；
  具体 V4/V6 字面量 → 直接用（`:527` / `:572`）。
  注意 `:492` 的 `addrs.pop()` 只取**最后一个**解析地址。
- `try_connect_to_url`（`:586`）：URL 级退避 `[1000,2000,4000]`；
  `:599` 与 `:603` 两处按 `has_directly_connected_conn` 早退；最终失败把 URL
  拉黑 300s（`DIRECT_CONNECTOR_BLACKLIST_TIMEOUT`）。
- `connect_to_url_once`（`:623`）：udp scheme 时按 host 家族分派到
  `connect_public_ipv6`（`:746`）/ `connect_public_ipv4`（`:711`，失败回退
  `connect_ordinary`）/ `connect_ordinary`（`:650`）。
- `connect_ordinary`（`:650`）用 `self.options.socket_context(transport, IpVersion::Both)`
  （`:665`）→ **域名 listener 的解析不限定族**；
  `resolve_url_addrs`（`easytier-core/src/connectivity/manual/mod.rs:1017`）按
  `context.ip_version` 过滤结果（`:1038`），所以把 `IpVersion` 换成 V4/V6
  就能把域名解析限定到某一族。

### 3.4 对称 NAT 打洞

`easytier-core/src/connectivity/hole_punch/`：

- `peer_adapters.rs:40-64` `UdpHolePunchPeerSource::candidates()`：
  `has_direct_connection: peer_map.has_peer(route.peer_id)`（`:59`）。
- `peer_adapters.rs:156` 附近 `TcpHolePunchPeerSource::candidates()`：同样是
  `has_peer`。
- 消费点：`udp/task.rs:63`、`tcp.rs:774` —— `if is_blacklisted(peer_id) ||
  candidate.has_direct_connection { return None; }`。
- UDP 对称打洞本身是 V4-only（`udp/runtime.rs:327` 固定 `IpVersion::V4`）；
  TCP 对称打洞两族都支持（`tcp.rs:266`）。
- `PeerMap` 在 `easytier-core/src/peers/conn/peer_map.rs:39`（不在
  `peer_manager.rs`）；`has_peer` 在 `:143`。

### 3.5 配置面

- `easytier-proto/proto/common.proto` `message Flags`（`:8`）最大字段号 **44**
  （`prefer_peer_relay = 44`）→ 新号 45。
- `easytier-proto/proto/api_manage.proto` `NetworkConfig` 最大字段号 **76**
  （`dhcp_ipv6_cidr = 76`）→ 新号 77。
- `easytier-proto/proto/api_config.proto` `message InstanceConfigPatch`（`:16`）
  最大字段号 **18**（`enable_manual_routes = 18`）→ 新号 19。
- `easytier-proto/build/flags.rs`：每个 `common.Flags` 字段必须带
  `(easytier.flag)` 注解否则 build 失败；`api` 拼写给出的字段必须在
  `api.manage.NetworkConfig` 存在同名，否则 build 失败；`form: true` 同样要求
  `NetworkConfig` 带同名字段；生成的 `FORM` 列表收
  `!deprecated && api_carries(api_name) && (Kind::Bool || form: true)` 的
  **Flags 字段名**（用作 `[flags]` 下的 TOML 键）。
- `easytier-core/src/config/api_input.rs`：`set_network_flags`（`:61`）把 Flags
  映射到 NetworkConfig；`gen_config`（`:503`）反向产出 FlagsPatch；
  `FORM_MANAGED_TOML_FIELDS`（`:116`）只列 TOML 顶层键，**本需求无需改**。
- `easytier-core/src/management/full/config_patch.rs:115-119`：把
  `InstanceConfigPatch` 落到实例配置（`patch` 是 `InstanceConfigPatch`，
  不是 NetworkConfig）。
- `easytier-core/src/wasi/web_client.rs:132`：wasi 客户端透传 flags。
- `easytier/src/instance/runtime_host.rs:140`：某些 flag 变更会触发
  `set_flags`。
- `easytier-web/src/client_manager/runtime_reconcile.rs:355-365`：网页配置
  保存时用 `FlagsPatch` 做差分，目前只处理 `disable_relay_data` 与
  `prefer_peer_relay`。
- CLI：`easytier/src/core.rs:1111` 从 proto Flags 描述符自动生成 clap 参数，
  参数帮助文本走 i18n `core_clap.<字段名>`（`:1099`），需要
  `easytier/locales/app.yml` 有对应条目。
- TOML：`easytier-core/src/config/toml.rs` 的 `TomlConfig.flags: FlagsPatch`
  支持 `[flags]` 段写任意 Flags 字段，**自动支持**。
- 前端：
  - `easytier-web/frontend-lib/src/types/network.ts`：`annotatedFlags`（`:117`）、
    `formFlags()`（`:154`）**只收 bool 字段**；`newFlagValues()`（`:161`）只给
    bool 填默认值；`defaultFlagValue()`（`:139`）也只处理 bool。
    非 bool 字段在 `DEFAULT_NETWORK_CONFIG()`（`:170`）里没有对应项。
  - `easytier-web/frontend-lib/src/components/Config.vue:123-133` 的 `bool_flags`
    渲染的是 **Checkbox**（模板 `:379`）—— 仓库里**没有**非 bool/三态 flag 的
    既有前端范式（`SelectButton` 只用在端口转发协议上，`:584`/`:630`）。
    本需求要新增一个三态控件。
  - 前端 proto 生成物 `easytier-web/frontend-lib/src/generated/proto/*.ts`
    **已入库**，改 proto 后必须 `pnpm codegen:proto`（`frontend-lib/package.json`）
    并提交。Rust 侧无需提交生成物（`easytier-proto/src/common.rs:23`
    的 `Flags::flags()` 运行期读描述符，build 产物不入库）。
  - 前端测试的字段清单（`tests/config-ui.spec.ts` 的 `CONFIG_FLAG_FIELDS` /
    `CONFIG_CHECKBOX_FIELDS` / `CONFIG_TOGGLE_FIELDS`、
    `tests/remote-management-config.spec.ts` 的 `BOOLEAN_CONFIG_FIELDS`）
    **都是 bool 清单**，新字段是 string，**不要**加进去，要另写三态控件用例。
  - `easytier-gui` 复用 `easytier-frontend-lib`（`easytier-gui/src/modules/api.ts:1`），
    但桌面 GUI 本身没有网络配置表单，只有网页配置服务器共用 `Config.vue`。

### 3.6 运行期能不能改 flags

能。`PeerContext::flags()`（`easytier-core/src/peers/context.rs:553`）默认返回
`Flags::default()`；`CorePeerContext::flags()`（`:747`）返回
`self.snapshot().flags.clone()`；运行期配置提交会重建快照
（`easytier-core/src/peers/peer_manager.rs:1344` `update_runtime_config_with`
→ `runtime_config.replace_with_current`）→ `flags()` 立刻返回新值。
既有热改先例：`easytier/src/instance/runtime_host.rs:140`、
`easytier-web/src/client_manager/runtime_reconcile.rs:355`。

**结论：这不是"启动期参数"，运行期改也能生效**（但需要下面的缓存刷新配合）。

## 4. 设计

### 4.1 配置面

字段名：`direct_conn_family`，取值 `auto` / `prefer_ipv4` / `prefer_ipv6`。

| 位置 | 改动 |
| --- | --- |
| `easytier-proto/proto/common.proto` | `Flags` 新增 `string direct_conn_family = 45 [(easytier.flag) = { default: "auto", form: true }];` |
| `easytier-proto/proto/api_manage.proto` | `NetworkConfig` 新增 `optional string direct_conn_family = 77;` |
| `easytier-proto/proto/api_config.proto` | `InstanceConfigPatch` 新增 `optional string direct_conn_family = 19;` |
| `easytier-core/src/config/api_input.rs` | `set_network_flags`（`:61`）加 `result.direct_conn_family = flags.direct_conn_family;`；`gen_config`（`:503`）加 `direct_conn_family: self.direct_conn_family.clone(),` |
| `easytier-core/src/management/full/config_patch.rs` | `:115-119` 加 `direct_conn_family: patch.direct_conn_family,`（patch 是 `InstanceConfigPatch`） |
| `easytier-core/src/wasi/web_client.rs` | `:132` 附近加 `direct_conn_family: config.direct_conn_family,` |
| `easytier/src/instance/runtime_host.rs` | `:140` 的条件里纳入新字段（与 `prefer_peer_relay` 同等对待） |
| `easytier-web/src/client_manager/runtime_reconcile.rs` | `:355-365` 的 FlagsPatch 差分纳入新字段 |
| `easytier/locales/app.yml` | 新增 `core_clap.direct_conn_family` 中英文条目 |
| `easytier-web/frontend-lib/src/generated/proto/**` | `pnpm codegen:proto` 重新生成并提交 |
| `easytier-web/frontend-lib/src/components/Config.vue` | 新增三态控件（详见 §4.1.1） |
| `easytier-web/frontend-lib/src/locales/cn.yaml` / `en.yaml` | 控件文案 |

TOML `[flags] direct_conn_family = "prefer_ipv4"` 与 CLI
`--direct-conn-family prefer_ipv4` 由 `Flags` 机制自动获得。

#### 4.1.1 前端三态控件

仓库里没有非 bool flag 的既有范式（§3.5），所以新增：

- 放在 `Config.vue` 的高级设置区域，用 `SelectButton`（仓库已有依赖，
  端口转发协议在用）呈现三个选项：`auto` / `prefer_ipv4` / `prefer_ipv6`。
- **默认值来源**：`formFlags()` / `newFlagValues()` / `defaultFlagValue()` 都只
  处理 bool（`network.ts:139`/`:154`/`:161`），所以本控件要自己处理：
  - 空串 / `undefined` → 视为 `auto`；
  - 声明默认值从 `flag.meta.default`（`"auto"`）读，不硬编码。
- 提交时把选择写进 `NetworkConfig.direct_conn_family`；读取时优先
  `NetworkConfig.direct_conn_family`，为空则回落 `auto`。
- 前端测试**不要**把该字段加进 bool 清单（`CONFIG_FLAG_FIELDS` /
  `CONFIG_CHECKBOX_FIELDS` / `CONFIG_TOGGLE_FIELDS` /
  `BOOLEAN_CONFIG_FIELDS`），另写三态控件用例。

### 4.2 族判定

**唯一谓词**（全链路只保留这一个，避免各处口径不一）：

```rust
// easytier-core/src/peers/conn/peer.rs（或同层新文件）
pub enum ConnFamily { Ipv4, Ipv6, Unknown }

impl Peer {
    /// 是否存在未关闭、且族为 family 的连接。
    /// 注意：**包含打洞得到的连接**（UDP/TCP 对称打洞、direct 打洞），
    /// 与 Peer::has_directly_connected_conn 的 !is_hole_punched() 过滤不同。
    pub fn has_conn_of_family(&self, family: ConnFamily) -> bool;
}
```

族判定实现：

1. `PeerConn` 新增访问器（`tunnel_info` 是私有字段，`get_conn_info()` 在握手前
   会 panic，都不能用）：

   ```rust
   impl PeerConn {
       /// 该连接对端的地址族。无法判定时为 Unknown。
       pub fn remote_host_family(&self) -> ConnFamily;
   }
   ```

2. 取 `self.tunnel_info` → `effective_remote_addr()`
   （`easytier-proto/src/common.rs:518`，已存在，直接用）→
   `Url` 的 host 家族。
3. `Url` 侧新增三态判定（把 `infer_tunnel_ipv6`（`common.rs:405`）的
   `Option<bool>` 升级为三态，避免重复实现）：

   ```rust
   pub enum TunnelHostFamily { Ipv4, Ipv6, Unknown }
   impl Url {
       pub fn tunnel_host_family(&self) -> TunnelHostFamily;
   }
   ```

   `Host::Ipv4` → Ipv4，`Host::Ipv6` → Ipv6，`Host::Domain` → Unknown。
4. `Unknown`（域名等）的语义：
   - **发起层**：`Unknown` **不算**偏好族连接（否则域名连接会永久压住偏好族
     的尝试）。
   - **选路层**：`Unknown` 与偏好族**同级**（排在偏好族之后、非偏好族之前），
     避免把未知类型的连接挤到最后。

`PeerMap`（`peer_map.rs:39`）新增 `has_conn_of_family(peer_id, family) -> bool`
（内部委托 `Peer`），`PeerManagerCore`（`peer_manager.rs`）转发同名方法
（`peer_manager.rs:1575` 的 `has_directly_connected_conn` 旁边）。
peer 不在本地 map 时（外网 peer）返回 `false`。

### 4.3 选路层

`easytier-core/src/peers/conn/peer.rs`：

- 排序键扩成分量式，**分量顺序不可调换**：

  ```rust
  fn conn_sort_key(latency_us: u64, is_hole_punched: bool, family: ConnFamily,
                   prefer: ConnFamilyPref) -> (bool, u8, u64)
  ```

  1. `(is_hole_punched && latency_us == 0)` —— 保持上游语义：刚打洞进来、
     首个 ping 还没确认的连接不抢流量。**放在第一位**，族偏好不会让一条
     还没被确认的连接立刻接管流量。
  2. 族排位（`u8`）：
     - `Auto` → 全部 0；
     - `prefer_ipv4` → V4=0、Unknown=1、V6=2；
     - `prefer_ipv6` → V6=0、Unknown=1、V4=2。
  3. `latency_us` —— 同族内仍按延迟。

- **`Auto` 时与上游逐位一致**：第二分量恒为 0 ⇒ `(bool, u8, u64)` 与上游
  `(bool, u64)` 同序；`min_by_key` 取首个最小值、并列保持先出现者 ⇒ 族分量
  不改变并列关系。
- **不会让 prefer_ipv4 落空**（重要）：排序键第一分量依赖
  `is_hole_punched()`，而它只看 origin（`peer_conn.rs:441`）。直连连接器打的
  IPv4 洞（`connect_public_ipv4`，`direct/mod.rs:711`）的 origin 是 `Direct`
  （`direct/mod.rs:876`）→ `is_hole_punched() == false` → 第一分量恒为 false
  → 该连接不被压制，第二分量立即让它成为默认连接。
  只有 `hole_punch` **模块**产出的连接（origin = `TcpHolePunch`/`UdpHolePunch`）
  才受第一分量压制，且只在"首个 ping 未确认"的窗口内。
- `select_conn`（`:214`）改用新键。
- 偏好值来源与刷新：
  - `Peer` 持 `context: ArcPeerContext`（`peer.rs:37`），`flags()` 会 clone
    整个 protobuf 消息，**不能放在热路径**。
  - `Peer` 内缓存 `prefer: ConnFamilyPref`，在 `default_conn_clear_task`
    （`:132`，每 5 秒一轮）里顺带刷新一次 —— 运行期改 flags 后最多 5 秒
    生效，同时覆盖 §5 的"运行期可改"。
- 生效延迟：`default_conn` 缓存本身也是 5 秒失效（`:132`），族偏好切换后
  最多 5 秒生效。

### 4.4 发起层（直连连接器）

`easytier-core/src/connectivity/direct/mod.rs`：

- 偏好值的读取：`DirectConnectorData`（`:163`）持有 `Arc<PeerManagerCore>`，
  但 `PeerManagerCore::context` 是私有字段（`peer_manager.rs:777`）→
  在 `PeerManagerCore` 上新增公开访问器（如
  `pub fn direct_conn_family(&self) -> ConnFamilyPref`，与 `:1478` 的
  `p2p_policy_flags()` 同风格），由直连连接器在决策点读取 → **运行期改
  flags 立即生效**，不必重启实例。
  （`DirectConnectorOptions` 仍可保留一个构造期的同名字段仅供参考，但**不是**
  决策依据。）
- **五处**"已有直连就放弃"改为族感知（偏好为 `Auto` 时全部保持原样）：

  | 位置 | 现状 | 改后（偏好非 Auto） |
  | --- | --- | --- |
  | `:349` `collect_peers_need_task` 过滤 | `!has_directly_connected_conn(route.peer_id)` | `!has_conn_of_family(route.peer_id, prefer_family)` |
  | `:413` `try_direct_connect` 循环内 | 早退 | 同上 |
  | `:468` `try_direct_connect_with_ip_list` 每组结束 | 早退 | 同上 |
  | `:599` `try_connect_to_url` 每轮前 | 早退 | 同上 |
  | `:603` `try_connect_to_url` 结果判定 | `result.is_ok() || has_directly_connected_conn(...)` | `result.is_ok() || has_conn_of_family(...)` |

  统一用 §4.2 的 `has_conn_of_family`（**含打洞连接**），不要沿用
  `has_directly_connected_conn` 的 `!is_hole_punched()` 过滤，否则
  prefer_ipv4 + 对称 UDP 打洞成功后判定恒为 false → 一直重试。
- **候选地址按族过滤**，过滤点放在 `spawn_direct_connect_tasks`（`:475`）内、
  解析之后（那里已拿到 `listener_host: Option<SocketAddr>`，`:492` 附近）：

  | listener 形态 | prefer_ipv4 | prefer_ipv6 |
  | --- | --- | --- |
  | 具体 IPv4 字面量 | 保留 | 丢弃 |
  | 具体 IPv6 字面量 | 丢弃 | 保留 |
  | `0.0.0.0`（unspecified） | **保留**（V4 展开源：`:507`） | 丢弃 |
  | `[::]`（unspecified） | 丢弃 | **保留**（V6 展开源：`:537`） |
  | 域名 | 不在此处过滤 | 不在此处过滤 |

  - **unspecified 形态必须保留**：它正是该族的候选展开源
    （`0.0.0.0` → `interface_ipv4s` + `public_ipv4`；`[::]` →
    `interface_ipv6s` + `public_ipv6`）。按字面量过滤会把偏好族唯一的
    候选源删掉。
  - 注意上游 `:433` 只按 `enable_ipv6` 排除 IPv6，**不排除具体 IPv4** → 所以
    prefer_ipv6 时具体 V4 listener 仍会被尝试，这正是本过滤要补的洞。
  - **域名 listener 无法在此处控族**：`:492` 的 `addrs.pop()` 只取最后一个
    解析地址，DNS 解析不按族过滤，且具体地址分支传下去的是**原始域名 URL**
    （`:534`/`:577`），最终由 `connect_ordinary`（`:650`）用
    `IpVersion::Both`（`:665`）重新解析 → 实际建连族不受此过滤控制。
    域名情形通过 §4.4.1 的 `IpVersion` 限定来控族。
- 域名/普通连接的族限定（§4.4.1）：`connect_ordinary`（`:650`）用
  `self.options.socket_context(transport, IpVersion::Both)`（`:665`）→
  偏好非 Auto 时改成对应族（`IpVersion::V4` / `IpVersion::V6`）→
  `resolve_url_addrs`（`manual/mod.rs:1017`）按 `context.ip_version` 过滤
  （`:1038`）→ 域名只解析偏好族。

#### 4.4.1 族限定的传递

`DirectConnectorOptions::socket_context(transport, ip_version)`（`:151`）
已经支持指定族，`IpVersion` 只有一个调用点需要改（`:665`）：

```rust
let ip_version = match prefer {
    ConnFamilyPref::Auto     => IpVersion::Both,
    ConnFamilyPref::PreferIpv4 => IpVersion::V4,
    ConnFamilyPref::PreferIpv6 => IpVersion::V6,
};
```

`connect_public_ipv4`（`:711`）/ `connect_public_ipv6`（`:746`）本身已绑定
对应族，无需改动。

### 4.5 对称 NAT 打洞层

`easytier-core/src/connectivity/hole_punch/peer_adapters.rs`：

- `UdpHolePunchPeerSource::candidates()`（`:40-64`）的
  `has_direct_connection: peer_map.has_peer(route.peer_id)`（`:59`）：
  - `Auto`：不变。
  - `prefer_ipv4`：改为"该 peer 已有 **V4** 连接"
    （UDP 对称打洞只能产出 V4，语义正好匹配）。
  - `prefer_ipv6`：**保持原样**（UDP 打洞产不出 V6 路径，现状即"已有任意连接
    就不打洞"）。这是全局表述"偏好族下不因已有非偏好族连接而跳过打洞"的
    **唯一例外**，其余情况都按偏好族放宽。
- `TcpHolePunchPeerSource::candidates()`（`:156` 附近）：TCP 两族都支持，
  偏好非 Auto 时统一改为"该 peer 已有偏好族连接"。
- 消费点（`udp/task.rs:63`、`tcp.rs:774`）**不改**：它们只看
  `candidate.has_direct_connection` 的真假，语义变化由上游取值负责。
- 依赖 §4.2 的 `PeerMap::has_conn_of_family(peer_id, family)`。

### 4.6 重试预算与冷却

偏好族打不通时必须有上限，否则 `try_direct_connect`（`:370`）的无限循环
（退避封顶 60s，永不结束）会一直重试。

- **窗口内轮次**：偏好非 Auto 时，`try_direct_connect`（`:370`）的外层循环
  最多跑 **3 轮**；每轮沿用现有退避的前 3 项（1s / 2s / 2s）与
  `try_connect_to_url` 自己的 `[1000, 2000, 4000]`。
- **窗口间隔**：一个窗口（3 轮，约 15 秒）失败后进入 **120 秒冷却**，冷却
  结束再来下一个窗口。
  - 120s 是刻意选的：上游在这种情况下是 60s 间隔无限重试，冷却不能比上游
    更差（早先草稿里的 10 分钟会明显劣于上游，WiFi 掉线恢复后要等 10 分钟）。
- **总上限**：连续 **5 个窗口**（约 5×15 + 4×120 ≈ 9 分钟）仍失败 → 停止对
  该 peer 的偏好族尝试，直到：偏好设置变化、或该 peer 的连接集合变化。
  → 相对上游"永久重试"是明确的收敛行为。
- **状态**：新增 per-peer 状态（与既有 `peer_blacklist`（`direct/mod.rs:177`，
  3600s，只装 `InvalidServiceKey`）、`listener_blacklist`（URL 级 300s）
  **语义不同**，不要复用），记录：窗口计数、窗口起始时间、冷却到期时间。
  放在 `DirectConnectorData`（`:163`）内。
- **两个入口都要查**（否则每 5 秒 `DIRECT_TASK_LOOP_INTERVAL_MS` 就会重新起
  任务，白跑）：
  - `collect_peers_need_task`（`:322`）—— 冷却/已放弃的 peer 不再排任务；
  - `try_direct_connect`（`:370`）入口 —— 复查状态。
- **成功即清零**：偏好族连接一旦建立（`has_conn_of_family` 为真），清空该
  peer 的窗口计数与冷却。
- `Auto`：**不引入**轮次上限与冷却，保持上游的无限重试语义。

### 4.7 运行时一致性小结

| 关注点 | 结论 |
| --- | --- |
| 打洞得到的连接算不算"偏好族连接" | **算**（§4.2 的 `has_conn_of_family` 含打洞连接） |
| 直连打洞的 IPv4 连接会不会被排序键第一分量压制 | **不会**（origin 是 `Direct`，`is_hole_punched()` 为假，§4.3） |
| `Unknown`（域名连接）在发起层算不算偏好族连接 | **不算**；在选路层与偏好族同级（§4.2） |
| 偏好族不通时会不会无限重试 | 不会：3 轮/窗口、120s 冷却、5 窗口后停止（§4.6） |
| 运行期改开关要不要重启 | 不用：flags 快照会重建（§3.6），但 `Peer` 的偏好缓存要每 5 秒刷新（§4.3）、直连连接器要读实时值（§4.4） |

## 5. 兼容性与降级

- **`auto` 是默认**：不设这个开关的用户，行为与上游一致 —— 排序键等价
  （§4.3）、发起侧五处判定不变、hole_punch 取值不变、无轮次上限与冷却。
- **降级链**：偏好族打不通 → 保留已有非偏好族连接继续用；偏好族打通 → 数据
  切到偏好族，非偏好族连接**不关闭**。
- **运行期可改**：网页配置保存后走 `runtime_reconcile.rs:355` 的 FlagsPatch
  差分（需纳入新字段），core 侧快照重建（§3.6）→ 最多 5 秒生效（§4.3）。
  界面无需提示"重启后生效"。
- **老配置兼容**：`NetworkConfig.direct_conn_family` 是 `optional`，老客户端
  不带该字段时按 `None`/空串 → `auto`。
- **proto 兼容性**：Flags 新号 45、NetworkConfig 新号 77、InstanceConfigPatch
  新号 19，均为新号，不影响既有字段。
- **前后端版本不一致**：前端控件按 `apiCarries('direct_conn_family')` 判断是否
  渲染（`network.ts:146`），后端不认识该字段时忽略。

## 6. 测试策略

**Rust 单元测试**

- `Url::tunnel_host_family()`：IPv4 / IPv6 / 域名 / 非法 URL 的取值。
- `PeerConn::remote_host_family()`：从 `tunnel_info` 取族；握手前调用**不 panic**。
- `Peer::has_conn_of_family`：含 UDP/TCP 对称打洞连接、含直连打洞连接；
  排除已关闭连接；`Unknown` 不匹配 V4/V6。
- 排序键：`Auto` 时新键与旧键结果完全一致（含并列顺序）；
  `prefer_ipv4` 时 V4 < Unknown < V6；`prefer_ipv6` 时反向；
  刚打洞未确认（`is_hole_punched && latency_us == 0`）的连接仍排最后。
- 发起层族过滤：prefer_ipv4 时具体 IPv6 listener 与 `[::]` 被剔除、
  `0.0.0.0` 保留；prefer_ipv6 时反之；`Auto` 时列表原样返回。
- 族限定的 `IpVersion`：prefer_ipv4 时 `connect_ordinary` 用 `IpVersion::V4`。
- 重试预算：3 轮后进入冷却、冷却期内 `collect_peers_need_task` 不排任务、
  120s 后进入下一窗口、5 个窗口后停止、成功后清零；`Auto` 时不设限。

**前端单元测试**

- 三态控件渲染三个选项、当前值正确回显。
- 空值 / 未知值 → 回落 `auto`。
- 提交后 `NetworkConfig.direct_conn_family` 正确；`toBackendNetworkConfig`
  带上该字段。
- **不**把该字段加进 `CONFIG_FLAG_FIELDS` / `CONFIG_CHECKBOX_FIELDS` /
  `CONFIG_TOGGLE_FIELDS` / `BOOLEAN_CONFIG_FIELDS`（都是 bool 清单），
  另写三态控件用例。

**集成 / 手工验收**：见 §8。

## 7. rebase 冲突面

本需求**必须**碰 Rust 核心与 proto —— 这与之前"改动尽量只落在 `easytier-gui/`"
的约定不同（那条约定是为 Android DNS 定制补丁定的，当时不需要动核心）。

热点文件清单：

- `easytier-proto/proto/common.proto`、`api_manage.proto`、`api_config.proto`
- `easytier-web/frontend-lib/src/generated/proto/**`（生成物，冲突时**重新
  codegen**，不手工解）
- `easytier-web/frontend-lib/src/components/Config.vue`
- `easytier-web/frontend-lib/src/types/network.ts`
- `easytier-core/src/config/api_input.rs`
- `easytier-core/src/instance/config.rs`
- `easytier-core/src/connectivity/direct/mod.rs`
- `easytier-core/src/connectivity/hole_punch/peer_adapters.rs`
- `easytier-core/src/peers/conn/peer.rs`、`peer_conn.rs`、`peer_map.rs`
- `easytier-core/src/peers/peer_manager.rs`
- `easytier/src/instance/runtime_host.rs`
- `easytier-web/src/client_manager/runtime_reconcile.rs`

同步上游时按既有"补丁维护机制"执行
（见 `docs/superpowers/specs/2026-09-02-android-exit-node-custom-vpn-dns-design.md` §2）。

## 8. 验收标准

1. 默认配置（未设置 `direct_conn_family`）下与上游行为一致：连接数、默认连接
   选择、重试节奏都不变。
2. `direct_conn_family = "prefer_ipv4"` 且本机同时有 V4 与 V6 直连时：
   默认连接是 V4；切到 V4 后不再回落到 V6（除非 V4 连接断开）。
3. 偏好族打不通时：3 轮尝试（约 15 秒）后进入 120 秒冷却，连续 5 个窗口
   （约 9 分钟）后停止尝试；日志能看到冷却与放弃信息；期间已有的 V6 连接
   一直可用。
4. 偏好族打通后：该 peer 的冷却/窗口计数被清零。
5. CLI `--direct-conn-family prefer_ipv4`、TOML `[flags] direct_conn_family`、
   网页配置页三态控件三者一致；运行期改开关后 5 秒内生效，无需重启实例。
6. IPv6 监听、中继、其它 flag 的行为不受影响。

## 9. 决策记录

已由项目所有者拍板：

1. **开关落在 proto Flags 全链路**（CLI + TOML + 网页配置页都能改），接受
   `api_manage.proto` / `api_config.proto` / codegen / `Config.vue` 的 rebase 成本。
2. **偏好族重试有上限**：3 轮/窗口 + 120 秒冷却 + 5 个窗口后停止，成功即清零
   （§4.6）。
3. **对称打洞一起放宽**：偏好族下若现有连接不是偏好族，不跳过打洞任务
   （例外：prefer_ipv6 下的 UDP 对称打洞，见 §4.5）。

由代码审阅补充、本版已采纳的决策：

4. **族判定只用 `effective_remote_addr()`**（`common.rs:518`），不重复实现
   "优先 resolved 再 remote"的逻辑（§4.2）。
5. **新增 `PeerConn` 族访问器**，不用 `get_conn_info()`（握手前 panic，§4.2）。
6. **`has_conn_of_family` 含打洞连接**，且全链路只用这一个谓词（§4.2、§4.7）。
7. **`Unknown` 在发起层不算偏好族，在选路层与偏好族同级**（§4.2）。
8. **族过滤按字面量/unspecified 区分**，保留 unspecified 展开源；域名靠
   `IpVersion` 限定（§4.4）。
9. **运行期可改**，不要写成"启动期参数"；配套要求 `Peer` 缓存 5 秒刷新
   （§3.6、§4.3、§4.4）。
10. **冷却 120 秒而非 10 分钟**（不能比上游的 60 秒无限重试更差，§4.6）。

留给实现者（不改变要做什么）：具体枚举类型的存放位置与命名、helper 的函数
签名细节、测试文件的组织方式、前端控件的具体布局、状态结构的字段名。
