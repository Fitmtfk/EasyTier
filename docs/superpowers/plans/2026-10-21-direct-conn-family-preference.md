# 直连族偏好（direct_conn_family）实施计划

**Goal:** 给 EasyTier 增加三值开关 `direct_conn_family`（`auto` / `prefer_ipv4` / `prefer_ipv6`），让「已连上 IPv6 但仍想用 IPv4 打洞、并让流量走 IPv4」的场景可用；同时保证 `auto` 与上游逐位一致，偏好族打不通时重试会收敛。

**Architecture:** 开关走 proto `Flags` 全链路（CLI + TOML + 网页配置页三处一致）。core 侧引入**单一族判定原语**：proto 侧三态 `TunnelHostFamily`（Ipv4 / Ipv6 / Unknown）、core 侧策略枚举 `ConnFamilyPref`（Auto / PreferIpv4 / PreferIpv6）、唯一谓词 `has_conn_of_family`（**含打洞连接**）。三个作用面消费它：选路（排序键加族分量）、发起（直连连接器五处「已有直连就放弃」判定族感知 + listener 族过滤 + `IpVersion` 限定）、对称打洞（跳过条件族感知）。偏好族不通时按「3 轮/窗口 + 120s 冷却 + 5 窗口上限」收敛。

**Tech Stack:** Rust（edition 2024，prost/protobuf）、Vue 3 + PrimeVue（frontend-lib）、vitest、pnpm 11

**Spec:** `docs/superpowers/specs/2026-10-21-direct-conn-family-preference-design.md`

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. 每个 Task 结束提交一次；每个 Task 完成后按 §Review Focus 复审。

---

## Global Constraints

1. 字段名固定 `direct_conn_family`；取值固定 `auto` / `prefer_ipv4` / `prefer_ipv6`；默认值 `auto`。
2. proto 新号固定：`common.Flags` = **45**、`api.manage.NetworkConfig` = **77**、`api_config.InstanceConfigPatch` = **19**。不得复用已占用字段号。
3. `common.Flags` 每个字段必须带 `(easytier.flag)` 注解；写了 `form: true` 就要求 `api.manage.NetworkConfig` 携带同名字段，否则 `easytier-proto/build/flags.rs` 构建失败。
4. **全链路只有一个族感知谓词**：`has_conn_of_family`。它**必须包含打洞得到的连接**（不得沿用 `has_directly_connected_conn` 的 `!is_hole_punched()` 过滤）。
5. `Unknown`（域名等无法判定的族）：**发起层不算**偏好族连接；**选路层与偏好族同级**（排在偏好族之后、非偏好族之前）。
6. `auto` 必须与上游**逐位一致**：排序键第二分量恒为 0；发起侧五处判定保持原样；无轮次上限与冷却；打洞层跳过条件保持原样。
7. 不得改动：IPv6 监听开关、中继/relay 选路、协议与线格式、退出节点行为。
8. 前端生成物 `easytier-web/frontend-lib/src/generated/proto/**` 必须用 `pnpm codegen:proto` 重新生成，**不要手工编辑生成物**。⚠️ 该目录被 `.gitignore:37` 忽略（`git ls-files` 为空），**不入库**，本地生成后确认字段存在即可。
9. 前端 spec 中的 `CONFIG_FLAG_FIELDS` / `CONFIG_CHECKBOX_FIELDS` / `CONFIG_TOGGLE_FIELDS` / `CONFIG_UI_BOOLEAN_FIELDS` **不得**加入新字段（它们都是 bool 清单）。
10. 每个 Task 至少一次 commit，message 用英文祈使句（如 `feat(core): add direct_conn_family flag`）。
11. Rust 侧无入库生成物：proto 改动只需 `cargo build -p easytier-proto`（`easytier-proto/src/common.rs` 的 `Flags::flags()` 运行期读描述符）。
12. 本机没有 `target/` 缓存 → Rust 冷编译很慢。**先 `cargo check -p easytier-core --lib` 再跑测试**；能跑 `cargo test -p easytier-core --lib <路径过滤>` 就别跑全量。
13. 跑前端测试/构建会弄脏 `easytier-gui/src/auto-imports.d.ts`、`easytier-gui/src/typed-router.d.ts`、`pnpm-lock.yaml` → 提交前 `git checkout --` 还原。
14. 禁止在本次改动里顺手重构无关代码；发现上游已有的问题记录下来，不要顺手改。

---

## Review Focus（这几条最容易咬人，复审时逐条查）

1. **`has_conn_of_family` 必须包含打洞连接。** 若误用 `has_directly_connected_conn` 的 `!is_hole_punched()` 过滤：`prefer_ipv4` + 对称 UDP 打洞成功后判定恒为 false → 无限重试 + listener 被拉黑（300s 黑名单），表现为「明明通了还在不停重连」。
2. **非法值必须回落 `auto`。** `direct_conn_family = "ipv4"` / `""` / `" prefer_ipv4 "`（前后空格）不得 panic、不得让实例起不来；统一 `trim()` 后解析，解析不到就 `Auto`。
3. **偏好族不通必须收敛。** 3 轮/窗口（约 15s）→ 120s 冷却 → 5 个窗口（约 9 分钟）后停止；期间另一族连接保持可用；`auto` 不引入任何上限（保持上游无限重试）。
4. **unspecified listener 必须保留。** `0.0.0.0` 是 V4 的唯一候选展开源（`direct/mod.rs:507`），`[::]` 是 V6 的唯一展开源（`:537`）；按字面量族过滤时不能把偏好族**自己**的展开源删掉。
5. **域名 listener 不能在字面量过滤层误杀。** 域名走 `IpVersion` 限定（`connect_ordinary`），字面量过滤层对域名必须放行。
6. **运行期改开关 5 秒内生效。** `Peer` 的偏好缓存每 5s 刷新；直连连接器读实时值；文档与实现都不得写成「需要重启实例」。
7. **`auto` 的排序键必须与上游同序**（`min_by_key` 取首个最小值、并列保持先出现者）。

---

## File Structure

**新增：**

- `easytier-core/src/peers/conn/family.rs` — `ConnFamily`（proto 三态的 re-export）、`ConnFamilyPref`（三值策略 + 解析 + 族排位）、`tunnel_info_family` 纯函数。
- `easytier-core/src/connectivity/direct/family_filter.rs` — listener 族过滤 + `IpVersion` 限定的纯函数。
- `easytier-core/src/connectivity/direct/family_budget.rs` — 偏好族重试预算与冷却状态机。

**修改：**

- proto：`easytier-proto/proto/common.proto`、`api_manage.proto`、`api_config.proto`
- proto 实现：`easytier-proto/src/common.rs`（`TunnelHostFamily` + `Url::tunnel_host_family`）
- core 配置面：`easytier-core/src/config/api_input.rs`、`easytier-core/src/management/full/config_patch.rs`、`easytier-core/src/wasi/web_client.rs`
- core 连接层：`easytier-core/src/peers/conn/peer.rs`、`peer_conn.rs`、`peer_map.rs`
- core 管理层：`easytier-core/src/peers/peer_manager.rs`
- core 发起层：`easytier-core/src/connectivity/direct/mod.rs`
- core 打洞层：`easytier-core/src/connectivity/hole_punch/peer_adapters.rs`
- CLI/运行时：`easytier/src/instance/runtime_host.rs`、`easytier-web/src/client_manager/runtime_reconcile.rs`、`easytier/locales/app.yml`
- 前端：`easytier-web/frontend-lib/src/components/Config.vue`、`easytier-web/frontend-lib/src/locales/{cn,en}.yaml`、`easytier-web/frontend-lib/src/generated/proto/**`（生成）、`easytier-web/frontend-lib/tests/direct-conn-family.spec.ts`（新测试）

---

### Task 0: 环境准备与基线

**Files:** 无（只跑命令）

**Steps:**

1. 确认工作区干净、���在 `releases/mine` 分支：

   ```bash
   git status --porcelain
   git branch --show-current   # 期望 releases/mine
   git log --oneline -3
   ```

2. 冷编译 core（**本机无 target 缓存，这一步最慢，耐心等**）：

   ```bash
   cargo check -p easytier-core --lib 2>&1 | tail -20
   ```

   记录耗时。如果超过 20 分钟仍在跑，就先只跑 `cargo check -p easytier-proto` 并把 core 的编译放到 Task 1 末尾做。

3. 记下基线测试数（用于最后对比「没有引入新失败」）：

   ```bash
   cargo test -p easytier-core --lib -- --list 2>/dev/null | tail -3
   ```

4. 前端环境确认：

   ```bash
   pnpm --version                      # 需要 11.x
   pnpm --dir easytier-web/frontend-lib build
   ```

   若 `pnpm` 不存在：`corepack enable pnpm`。

5. 把基线数字记在本次会话里（Task 8 要对比）。**不要**提交任何构建产物。

---

### Task 1: proto 字段与配置面全链路

**Files:**

- Modify: `easytier-proto/proto/common.proto`（`Flags` 末尾，字段号 45）
- Modify: `easytier-proto/proto/api_manage.proto`（`NetworkConfig` 末尾，字段号 77）
- Modify: `easytier-proto/proto/api_config.proto`（`InstanceConfigPatch` 末尾，字段号 19）
- Modify: `easytier-core/src/config/api_input.rs`（`set_network_flags` 约 :61；反向映射约 :503）
- Modify: `easytier-core/src/management/full/config_patch.rs`（约 :115）
- Modify: `easytier-core/src/wasi/web_client.rs`（约 :132）
- Modify: `easytier/src/instance/runtime_host.rs`（约 :140）
- Modify: `easytier-web/src/client_manager/runtime_reconcile.rs`（约 :355）
- Modify: `easytier/locales/app.yml`（`core_clap:` 段）
- Regenerate: `easytier-web/frontend-lib/src/generated/proto/**`

**Interfaces:**

- Produces: `common::Flags.direct_conn_family: String`（默认 `"auto"`）、`api_manage::NetworkConfig.direct_conn_family: Option<String>`、`api_config::InstanceConfigPatch.direct_conn_family: Option<String>`。
- Consumes: 无（后续 Task 消费）。

**Steps:**

1. **先写失败测试**（TDD）。在 `easytier-core/src/config/toml.rs` 的 `mod tests` 中，紧跟 `toml_round_trip_preserves_config_and_non_default_flags` 之后新增：

   ```rust
       #[cfg(feature = "config-write")]
       #[test]
       fn toml_round_trip_preserves_direct_conn_family() {
           let config = TomlConfig::new_from_str(
               r#"
   [flags]
   direct_conn_family = "prefer_ipv4"
   "#,
           )
           .unwrap();

           assert_eq!(config.get_flags().direct_conn_family, "prefer_ipv4");

           let restored = TomlConfig::new_from_str(&config.dump()).unwrap();
           assert_eq!(restored.get_flags().direct_conn_family, "prefer_ipv4");
       }
   ```

   运行：

   ```bash
   cargo test -p easytier-core --lib config::toml::tests::toml_round_trip_preserves_direct_conn_family
   ```

   期望：**编译失败**，报错里出现 `direct_conn_family` 找不到字段 → 证明测试确实约束了新字段。

2. 改 proto 三处：

   `easytier-proto/proto/common.proto`（`Flags` 末尾）：

   ```proto
     // Direct connection address family preference: auto / prefer_ipv4 / prefer_ipv6.
     string direct_conn_family = 45 [(easytier.flag) = { default: "auto", form: true }];
   ```

   `easytier-proto/proto/api_manage.proto`（`NetworkConfig` 末尾）：

   ```proto
     optional string direct_conn_family = 77;
   ```

   `easytier-proto/proto/api_config.proto`（`InstanceConfigPatch` 末尾）：

   ```proto
     optional string direct_conn_family = 19;
   ```

3. 校验 proto 构建（`build/flags.rs` 会强制检查注解与 api 字段对应关系）：

   ```bash
   cargo build -p easytier-proto
   ```

   期望成功。若报 `common.Flags.direct_conn_family carries no (easytier.flag) annotation` 或 `api.manage.NetworkConfig does not carry ...`，说明注解或字段号写错了。

4. `easytier-core/src/config/api_input.rs` 双向映射：

   - `set_network_flags`（Flags → NetworkConfig）里加一行：
     ```rust
     result.direct_conn_family = flags.direct_conn_family.clone();
     ```
   - 反向映射（NetworkConfig → FlagsPatch）里加一行：
     ```rust
     direct_conn_family: self.direct_conn_family.clone(),
     ```

   注意：`Flags.direct_conn_family` 是 `String`（proto3 非 optional），`NetworkConfig.direct_conn_family` 是 `Option<String>`；反向映射用 `self.direct_conn_family.clone()`（`Option<String>` → `Option<String>`）✓。

5. `easytier-core/src/management/full/config_patch.rs`：在构造 `NetworkConfig` 的字面量里加
   ```rust
   direct_conn_family: patch.direct_conn_family,
   ```
   （`patch` 是 `InstanceConfigPatch`，字段为 `Option<String>` ✓）

6. `easytier-core/src/wasi/web_client.rs`：加
   ```rust
   direct_conn_family: config.direct_conn_family,
   ```

7. `easytier/src/instance/runtime_host.rs:140`：把新字段纳入「这些 flag 变化需要额外处理」的条件：
   ```rust
   if patch.disable_relay_data.is_some()
       || patch.prefer_peer_relay.is_some()
       || patch.direct_conn_family.is_some()
   {
   ```

8. `easytier-web/src/client_manager/runtime_reconcile.rs:355` 附近：FlagsPatch 差分里纳入 `direct_conn_family`（与 `prefer_peer_relay` 同样处理），保证网页配置保存后能把差分下发给 core。

9. `easytier/locales/app.yml` 的 `core_clap:` 段新增条目（`en` / `zh-CN` 都要有，缺一个 `t!` 会取不到）：

   ```yaml
     direct_conn_family:
       en: "preferred address family for direct connections, one of: auto, prefer_ipv4, prefer_ipv6"
       zh-CN: "直连连接的地址族偏好，可选值：auto、prefer_ipv4、prefer_ipv6"
   ```

10. 重新生成前端 proto：

    ```bash
    cd easytier-web/frontend-lib
    pnpm codegen:proto
    git status --porcelain
    ```

    期望 `src/generated/proto/common.ts` 与 `src/generated/proto/api_manage.ts` 出现 `direct_conn_family`；**生成物在 .gitignore 内不入库**（`.gitignore:37`），本地确认字段存在即可，**不要手工改**。

11. 跑测试：

    ```bash
    cargo test -p easytier-core --lib config::toml::tests::toml_round_trip_preserves_direct_conn_family
    cargo check -p easytier-core --lib
    cargo check -p easytier 2>&1 | tail -5
    ```

12. 提交：

    ```bash
    git add easytier-proto/proto easytier-core/src/config/api_input.rs easytier-core/src/management/full/config_patch.rs easytier-core/src/wasi/web_client.rs easytier/src/instance/runtime_host.rs easytier-web/src/client_manager/runtime_reconcile.rs easytier/locales/app.yml
    git commit -m "feat(proto): add direct_conn_family flag for direct connection family preference"
    ```

---

### Task 2: 族判定原语（唯一谓词的地基）

**Files:**

- Modify: `easytier-proto/src/common.rs`（`infer_tunnel_ipv6` 约 :405；`impl Url` 约 :467）
- Create: `easytier-core/src/peers/conn/family.rs`
- Modify: `easytier-core/src/peers/conn/mod.rs`（加 `pub(crate) mod family;`）
- Modify: `easytier-core/src/peers/conn/peer_conn.rs`（加 `remote_host_family`）
- Modify: `easytier-core/src/peers/conn/peer.rs`（加 `has_conn_of_family`）
- Modify: `easytier-core/src/peers/conn/peer_map.rs`（加转发）
- Modify: `easytier-core/src/peers/peer_manager.rs`（加转发 + `direct_conn_family()`）

**Interfaces:**

- Produces:
  - `easytier_proto::common::TunnelHostFamily { Ipv4, Ipv6, Unknown }` + `Url::tunnel_host_family(&self) -> TunnelHostFamily`
  - `easytier_core::peers::conn::family::{ConnFamily, ConnFamilyPref, tunnel_info_family, conn_counts_as_family}`
  - `PeerConn::remote_host_family(&self) -> ConnFamily`
  - `Peer::has_conn_of_family(&self, family: ConnFamily) -> bool`
  - `PeerMap::has_conn_of_family(&self, peer_id: PeerId, family: ConnFamily) -> bool`
  - `PeerManagerCore::has_conn_of_family(&self, peer_id: PeerId, family: ConnFamily) -> bool`
  - `PeerManagerCore::direct_conn_family(&self) -> ConnFamilyPref`
- Consumes: Task 1 的 `Flags.direct_conn_family`。

**Steps:**

1. **先写失败测试**：

   a) `easytier-proto/src/common.rs` 内的测试模块（若无则新建 `#[cfg(test)] mod tests`）加：

   ```rust
   #[test]
   fn tunnel_host_family_detects_ip_and_domain() {
       let v4 = Url { url: "tcp://1.2.3.4:11010".to_string() };
       let v6 = Url { url: "tcp://[2001:db8::1]:11010".to_string() };
       let domain = Url { url: "tcp://example.com:11010".to_string() };
       let garbage = Url { url: "not a url".to_string() };

       assert_eq!(v4.tunnel_host_family(), TunnelHostFamily::Ipv4);
       assert_eq!(v6.tunnel_host_family(), TunnelHostFamily::Ipv6);
       assert_eq!(domain.tunnel_host_family(), TunnelHostFamily::Unknown);
       assert_eq!(garbage.tunnel_host_family(), TunnelHostFamily::Unknown);
   }
   ```

   b) 新建 `easytier-core/src/peers/conn/family.rs` 时同步写它的单元测试（见步骤 3 的代码，测试写在同一文件底部 `#[cfg(test)] mod tests`）。

   c) `easytier-core/src/peers/conn/peer.rs` 的测试模块加：

   ```rust
   #[test]
   fn family_predicate_excludes_closed_and_unknown() {
       assert!(conn_counts_as_family(ConnFamily::Ipv4, false, ConnFamily::Ipv4));
       assert!(!conn_counts_as_family(ConnFamily::Ipv4, true, ConnFamily::Ipv4));
       assert!(!conn_counts_as_family(ConnFamily::Unknown, false, ConnFamily::Ipv4));
       assert!(!conn_counts_as_family(ConnFamily::Unknown, false, ConnFamily::Ipv6));
   }
   ```

2. 跑测试确认失败：`cargo test -p easytier-core --lib family` → 编译失败（模块不存在）✓

3. **proto 侧三态**（`easytier-proto/src/common.rs`）：

   ```rust
   /// 隧道对端地址的族；无法判定（域名、非法 URL）为 Unknown。
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub enum TunnelHostFamily {
       Ipv4,
       Ipv6,
       Unknown,
   }
   ```

   在 `impl Url` 内新增：

   ```rust
       /// 对端地址的族；域名或无法解析的 URL 返回 Unknown。
       pub fn tunnel_host_family(&self) -> TunnelHostFamily {
           match infer_tunnel_ipv6(&self.url) {
               Some(true) => TunnelHostFamily::Ipv6,
               Some(false) => TunnelHostFamily::Ipv4,
               None => TunnelHostFamily::Unknown,
           }
       }
   ```

   复用已有的 `infer_tunnel_ipv6`（:405）—— 它已经区分了 `Some(true)=V6` / `Some(false)=V4` / `None=域名或非法`，**不要另写一套解析**。

4. **core 侧策略**（新建 `easytier-core/src/peers/conn/family.rs`）：

   ```rust
   use easytier_proto::common::{TunnelInfo, TunnelHostFamily};

   /// 连接的对端地址族。
   pub use easytier_proto::common::TunnelHostFamily as ConnFamily;

   /// 直连族偏好策略。
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
   pub enum ConnFamilyPref {
       /// 与上游一致：不做族偏好。
       #[default]
       Auto,
       PreferIpv4,
       PreferIpv6,
   }

   impl ConnFamilyPref {
       /// 解析配置值；空串、未知值、大小写不符都回落 Auto。
       pub fn from_flag_str(value: &str) -> Self {
           match value.trim() {
               "prefer_ipv4" => Self::PreferIpv4,
               "prefer_ipv6" => Self::PreferIpv6,
               _ => Self::Auto,
           }
       }

       /// 写回配置用的字符串。
       pub fn as_flag_str(self) -> &'static str {
           match self {
               Self::Auto => "auto",
               Self::PreferIpv4 => "prefer_ipv4",
               Self::PreferIpv6 => "prefer_ipv6",
           }
       }

       /// 偏好族；Auto 无偏好。
       pub fn preferred_family(self) -> Option<ConnFamily> {
           match self {
               Self::Auto => None,
               Self::PreferIpv4 => Some(ConnFamily::Ipv4),
               Self::PreferIpv6 => Some(ConnFamily::Ipv6),
           }
       }

       /// 族排位：偏好族 0，Unknown 1，另一族 2；Auto 全为 0。
       pub fn family_rank(self, family: ConnFamily) -> u8 {
           match self {
               Self::Auto => 0,
               _ => match self.preferred_family() {
                   Some(preferred) if family == preferred => 0,
                   Some(_) => match family {
                       ConnFamily::Unknown => 1,
                       _ => 2,
                   },
                   None => 0,
               },
           }
       }
   }

   /// 从 tunnel_info 取对端族；没有信息或域名 → Unknown。
   pub fn tunnel_info_family(tunnel_info: Option<&TunnelInfo>) -> ConnFamily {
       tunnel_info
           .and_then(|info| info.effective_remote_addr())
           .map(|url| url.tunnel_host_family())
           .unwrap_or(ConnFamily::Unknown)
   }

   /// 一条连接是否算作「已有 family 族的连接」：未关闭且族匹配。
   pub fn conn_counts_as_family(family: ConnFamily, closed: bool, wanted: ConnFamily) -> bool {
       !closed && family == wanted
   }
   ```

   同文件底部加测试：

   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;

       #[test]
       fn invalid_pref_falls_back_to_auto() {
           assert_eq!(ConnFamilyPref::from_flag_str("prefer_ipv4"), ConnFamilyPref::PreferIpv4);
           assert_eq!(ConnFamilyPref::from_flag_str(" prefer_ipv6 "), ConnFamilyPref::PreferIpv6);
           assert_eq!(ConnFamilyPref::from_flag_str("ipv4"), ConnFamilyPref::Auto);
           assert_eq!(ConnFamilyPref::from_flag_str(""), ConnFamilyPref::Auto);
           assert_eq!(ConnFamilyPref::default(), ConnFamilyPref::Auto);
       }

       #[test]
       fn family_rank_orders_preferred_then_unknown_then_other() {
           let v4 = ConnFamilyPref::PreferIpv4;
           assert_eq!(v4.family_rank(ConnFamily::Ipv4), 0);
           assert_eq!(v4.family_rank(ConnFamily::Unknown), 1);
           assert_eq!(v4.family_rank(ConnFamily::Ipv6), 2);

           let v6 = ConnFamilyPref::PreferIpv6;
           assert_eq!(v6.family_rank(ConnFamily::Ipv6), 0);
           assert_eq!(v6.family_rank(ConnFamily::Unknown), 1);
           assert_eq!(v6.family_rank(ConnFamily::Ipv4), 2);

           assert_eq!(ConnFamilyPref::Auto.family_rank(ConnFamily::Ipv4), 0);
           assert_eq!(ConnFamilyPref::Auto.family_rank(ConnFamily::Ipv6), 0);
       }

       #[test]
       fn tunnel_info_family_reads_effective_remote_addr() {
           let v4 = TunnelInfo {
               remote_addr: Some(easytier_proto::common::Url { url: "tcp://10.0.0.1:11010".into() }),
               ..Default::default()
           };
           assert_eq!(tunnel_info_family(Some(&v4)), ConnFamily::Ipv4);
           assert_eq!(tunnel_info_family(None), ConnFamily::Unknown);
       }
   }
   ```

5. `easytier-core/src/peers/conn/mod.rs`：加模块声明（与 `mod peer;` / `mod peer_conn;` 同级）：

   ```rust
   pub(crate) mod family;
   ```

6. `easytier-core/src/peers/conn/peer_conn.rs`：加访问器（放在 `is_hole_punched` 附近）：

   ```rust
       /// 该连接对端的地址族；无法判定时为 Unknown。
       /// 不走 get_conn_info()，握手前调用不会 panic。
       pub fn remote_host_family(&self) -> ConnFamily {
           tunnel_info_family(self.tunnel_info.as_ref())
       }
   ```

   并在文件顶部 import：`use crate::peers::conn::family::{tunnel_info_family, ConnFamily};`

7. `easytier-core/src/peers/conn/peer.rs`：加谓词（放在 `has_directly_connected_conn` 旁边，:289 附近）：

   ```rust
       /// 是否存在未关闭、且族为 family 的连接。
       /// 与 has_directly_connected_conn 不同：**包含打洞得到的连接**。
       pub fn has_conn_of_family(&self, family: ConnFamily) -> bool {
           self.conns.iter().any(|entry| {
               let conn = entry.value();
               conn_counts_as_family(conn.remote_host_family(), conn.is_closed(), family)
           })
       }
   ```

8. `easytier-core/src/peers/conn/peer_map.rs`：加转发（放在 `has_peer` 旁边，:143 附近）：

   ```rust
       /// peer 不在本地 map 时返回 false。
       pub fn has_conn_of_family(&self, peer_id: PeerId, family: ConnFamily) -> bool {
           self.get_peer_by_id(peer_id)
               .map(|peer| peer.has_conn_of_family(family))
               .unwrap_or(false)
       }
   ```

9. `easytier-core/src/peers/peer_manager.rs`：加两个方法（放在 `has_directly_connected_conn` 旁边，:1575 附近）：

   ```rust
       /// 该 peer 是否已有指定族的连接（含打洞得到的连接）。
       pub fn has_conn_of_family(&self, peer_id: PeerId, family: ConnFamily) -> bool {
           if let Some(peer) = self.peers.get_peer_by_id(peer_id) {
               peer.has_conn_of_family(family)
           } else {
               self.foreign_network_client
                   .get_peer_map()
                   .has_conn_of_family(peer_id, family)
           }
       }

       /// 当前生效的直连族偏好（实时读取，支持运行期修改）。
       pub fn direct_conn_family(&self) -> ConnFamilyPref {
           ConnFamilyPref::from_flag_str(&self.context.flags().direct_conn_family)
       }
   ```

   import：`use crate::peers::conn::family::{ConnFamily, ConnFamilyPref};`

10. 跑测试：

    ```bash
    cargo test -p easytier-core --lib family
    cargo check -p easytier-core --lib
    ```

11. 提交：

    ```bash
    git add easytier-proto/src/common.rs easytier-core/src/peers/conn
    git commit -m "feat(core): add connection family primitives and has_conn_of_family predicate"
    ```

---

### Task 3: 选路层族偏好

**Files:**

- Modify: `easytier-core/src/peers/conn/peer.rs`（`conn_latency_sort_key` :30；`Peer` 结构 :34-53；`Peer::new` :55；`default_conn_clear_task` :132-141；`select_conn` :214-238）

**Interfaces:**

- Produces: `conn_sort_key(latency_us, is_hole_punched, family, prefer) -> (bool, u8, u64)`；`Peer` 内偏好缓存。
- Consumes: Task 2 的 `ConnFamily` / `ConnFamilyPref`。

**Steps:**

1. **先写失败测试**。改写 `easytier-core/src/peers/conn/peer.rs` 现有测试模块（:342 起，现有用 `conn_latency_sort_key`）为：

   ```rust
   #[test]
   fn auto_preference_matches_upstream_ordering() {
       use ConnFamily::*;
       let auto = ConnFamilyPref::Auto;
       // 上游键：(is_hole_punched && latency == 0, latency)
       let upstream = |latency, punched| (punched && latency == 0, latency);
       for (latency, punched) in [(0u64, true), (0, false), (5_000, true), (20_000, false)] {
           for family in [Ipv4, Ipv6, Unknown] {
               let new_key = conn_sort_key(latency, punched, family, auto);
               assert_eq!(new_key.0, upstream(latency, punched).0);
               assert_eq!(new_key.2, upstream(latency, punched).1);
               assert_eq!(new_key.1, 0);
           }
       }
   }

   #[test]
   fn preferred_family_ranks_before_unknown_and_other_family() {
       let v4 = ConnFamilyPref::PreferIpv4;
       assert!(conn_sort_key(20_000, false, ConnFamily::Ipv4, v4) < conn_sort_key(0, false, ConnFamily::Unknown, v4));
       assert!(conn_sort_key(0, false, ConnFamily::Unknown, v4) < conn_sort_key(0, false, ConnFamily::Ipv6, v4));

       let v6 = ConnFamilyPref::PreferIpv6;
       assert!(conn_sort_key(20_000, false, ConnFamily::Ipv6, v6) < conn_sort_key(0, false, ConnFamily::Unknown, v6));
       assert!(conn_sort_key(0, false, ConnFamily::Unknown, v6) < conn_sort_key(0, false, ConnFamily::Ipv4, v6));
   }

   #[test]
   fn unconfirmed_hole_punch_still_goes_last() {
       let v4 = ConnFamilyPref::PreferIpv4;
       // 刚打洞进来、首个 ping 未确认的连接，即使族匹配也不抢流量
       assert!(conn_sort_key(0, true, ConnFamily::Ipv4, v4) > conn_sort_key(20_000, false, ConnFamily::Ipv6, v4));
   }
   ```

2. 跑测试 → 失败（`conn_sort_key` 不存在）✓

3. 在 `peer.rs` 新增排序键（保留 `conn_latency_sort_key` 不动，它是上游语义的锚点）：

   ```rust
   /// 连接排序键：分量顺序不可调换。
   /// 1. 刚打洞进来、首个 ping 未确认的连接排最后（上游语义）。
   /// 2. 族排位：偏好族 0、Unknown 1、另一族 2；Auto 恒 0（与上游同序）。
   /// 3. 同族内按延迟。
   fn conn_sort_key(
       latency_us: u64,
       is_hole_punched: bool,
       family: ConnFamily,
       prefer: ConnFamilyPref,
   ) -> (bool, u8, u64) {
       let (unconfirmed, latency) = conn_latency_sort_key(latency_us, is_hole_punched);
       (unconfirmed, prefer.family_rank(family), latency)
   }
   ```

4. `Peer` 结构加偏好缓存字段（`peer.rs:34-53` 的 struct 定义）：

   ```rust
       /// 直连族偏好缓存；每 5 秒刷新一次（见 default_conn_clear_task）。
       prefer_family: Arc<ArcSwap<ConnFamilyPref>>,
   ```

   `Peer::new`（:55）里初始化并启动刷新：

   ```rust
       let prefer_family = Arc::new(ArcSwap::from_pointee(
           ConnFamilyPref::from_flag_str(&context.flags().direct_conn_family),
       ));
       ...
       let prefer_family_copy = prefer_family.clone();
       let prefer_context_copy = context.clone();
       // 与 default_conn_clear_task 同频刷新：运行期改 flag 后最多 5 秒生效
       let prefer_refresh_task = AbortOnDropHandle::new(tokio::spawn(async move {
           loop {
               crate::foundation::time::sleep(std::time::Duration::from_secs(5)).await;
               let pref = ConnFamilyPref::from_flag_str(
                   &prefer_context_copy.flags().direct_conn_family,
               );
               prefer_family_copy.store(Arc::new(pref));
           }
       }));
   ```

   把 `prefer_refresh_task` 也存进 `Peer`（否则 task 句柄被丢弃，task 会立即被 abort —— `AbortOnDropHandle` drop 即 abort）。在 struct 里加同名字段并在 `new` 里赋值。

   > 若 `ArcSwap` 的类型参数需要 `RefCnt`（arc_swap 1.x 的 `ArcSwap<T>` 对任意 `T` 都可用，用 `ArcSwap::from_pointee` 构造）—— 若编译报错，改用 `Arc<ArcSwapOption<ConnFamilyPref>>` + `store(Some(Arc::new(pref)))`，语义相同。

5. `select_conn`（:214-238）改用新键：

   ```rust
       fn select_conn(&self) -> Option<ArcPeerConn> {
           // ...（保留原有 default_conn 缓存逻辑）
           let prefer = *self.prefer_family.load();
           let conn = self.conns.iter().min_by_key(|conn| {
               let conn = conn.value();
               conn_sort_key(
                   conn.get_stats().latency_us,
                   conn.is_hole_punched(),
                   conn.remote_host_family(),
                   prefer,
               )
           })?;
           // ...（保留原有写回缓存逻辑）
       }
   ```

6. 跑测试：

   ```bash
   cargo test -p easytier-core --lib peers::conn::peer
   ```

7. 提交：

   ```bash
   git add easytier-core/src/peers/conn/peer.rs
   git commit -m "feat(core): rank direct connections by preferred address family"
   ```

---

### Task 4: 发起层族感知（直连连接器）

**Files:**

- Create: `easytier-core/src/connectivity/direct/family_filter.rs`
- Modify: `easytier-core/src/connectivity/direct/mod.rs`（`collect_peers_need_task` :322-354；`try_direct_connect` :370-417；`try_direct_connect_with_ip_list` :419-473；`spawn_direct_connect_tasks` :475-584；`try_connect_to_url` :586-621；`connect_ordinary` :650-667）

**Interfaces:**

- Produces: `family_filter::{listener_matches_prefer_family, filter_listeners_by_prefer_family, prefer_ip_version}`。
- Consumes: Task 2 的 `ConnFamilyPref`；Task 1 的 flag（经 `PeerManagerCore::direct_conn_family()`）。

**Steps:**

1. **先写失败测试**（新建 `family_filter.rs` 时同步写测试）：

   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;

       #[test]
       fn auto_keeps_all_listeners() {
           for url in ["udp://0.0.0.0:11010", "udp://[::]:11010", "tcp://1.2.3.4:11010", "tcp://[2001:db8::1]:11010", "tcp://example.com:11010"] {
               assert!(listener_matches_prefer_family(url, ConnFamilyPref::Auto), "{url}");
           }
       }

       #[test]
       fn prefer_ipv4_keeps_ipv4_and_unspecified_ipv4_only() {
           assert!(listener_matches_prefer_family("udp://0.0.0.0:11010", ConnFamilyPref::PreferIpv4));
           assert!(listener_matches_prefer_family("tcp://1.2.3.4:11010", ConnFamilyPref::PreferIpv4));
           assert!(!listener_matches_prefer_family("udp://[::]:11010", ConnFamilyPref::PreferIpv4));
           assert!(!listener_matches_prefer_family("tcp://[2001:db8::1]:11010", ConnFamilyPref::PreferIpv4));
           // 域名不在此层过滤
           assert!(listener_matches_prefer_family("tcp://example.com:11010", ConnFamilyPref::PreferIpv4));
       }

       #[test]
       fn prefer_ipv6_keeps_ipv6_and_unspecified_ipv6_only() {
           assert!(listener_matches_prefer_family("udp://[::]:11010", ConnFamilyPref::PreferIpv6));
           assert!(listener_matches_prefer_family("tcp://[2001:db8::1]:11010", ConnFamilyPref::PreferIpv6));
           assert!(!listener_matches_prefer_family("udp://0.0.0.0:11010", ConnFamilyPref::PreferIpv6));
           assert!(!listener_matches_prefer_family("tcp://1.2.3.4:11010", ConnFamilyPref::PreferIpv6));
           assert!(listener_matches_prefer_family("tcp://example.com:11010", ConnFamilyPref::PreferIpv6));
       }
   }
   ```

2. **实现 `family_filter.rs`**：

   ```rust
   use std::net::Ipv4Addr;

   use url::Url;

   use crate::peers::conn::family::ConnFamilyPref;

   /// listener URL 是否允许在当前偏好下尝试。
   /// - Auto：全部允许。
   /// - 字面量 IP：只允许偏好族（`0.0.0.0` 属 V4、`[::]` 属 V6），
   ///   但**偏好族自己的 unspecified 展开源必须保留**。
   /// - 域名：不在此层过滤（由 IpVersion 限定控族）。
   pub fn listener_matches_prefer_family(listener_url: &str, prefer: ConnFamilyPref) -> bool {
       match prefer {
           ConnFamilyPref::Auto => true,
           _ => match Url::parse(listener_url).ok().and_then(|url| url.host()) {
               Some(url::Host::Ipv4(_)) => prefer == ConnFamilyPref::PreferIpv4,
               Some(url::Host::Ipv6(_)) => prefer == ConnFamilyPref::PreferIpv6,
               Some(url::Host::Domain(_)) | None => true,
           },
       }
   }

   /// 按偏好过滤 listener 列表。
   pub fn filter_listeners_by_prefer_family<T>(
       listeners: Vec<T>,
       prefer: ConnFamilyPref,
       url_of: impl Fn(&T) -> &str,
   ) -> Vec<T> {
       if prefer == ConnFamilyPref::Auto {
           return listeners;
       }
       listeners
           .into_iter()
           .filter(|listener| listener_matches_prefer_family(url_of(listener), prefer))
           .collect()
   }

   /// 直连时限定解析的地址族。
   pub fn prefer_ip_version(prefer: ConnFamilyPref) -> IpVersion {
       match prefer {
           ConnFamilyPref::Auto => IpVersion::Both,
           ConnFamilyPref::PreferIpv4 => IpVersion::V4,
           ConnFamilyPref::PreferIpv6 => IpVersion::V6,
       }
   }
   ```

   > `IpVersion` 从 core 现有的 socket 模块导入（`direct/mod.rs` 已在用 `IpVersion::Both`）。`url` crate 在 core 里可用（`direct/mod.rs` 已 `use url::{Host, Url};`）。
   >
   > `filter_listeners_by_prefer_family` 写成泛型 + 闭包，是为了能直接作用于 `Vec<proto::Url>`（proto 的 `Url` 只有 `url: String` 字段，`url_of` 取 `&item.url`）。

3. 在 `direct/mod.rs` 顶部加 `mod family_filter;` 并 import。

4. **五处判定族感知**。统一模式：

   ```rust
   // 偏好非 Auto 时，用「已有偏好族连接」代替「已有直连连接」；
   // Auto 时完全保持上游行为。
   fn peer_satisfied(connector: &DirectConnectorData, dst_peer_id: PeerId) -> bool {
       let prefer = connector.peer_manager.direct_conn_family();
       match prefer.preferred_family() {
           Some(family) => connector.peer_manager.has_conn_of_family(dst_peer_id, family),
           None => connector.peer_manager.has_directly_connected_conn(dst_peer_id),
       }
   }
   ```

   放置位置：`DirectConnectorData`（:163）所在 impl 之外的自由函数，或作为 `impl DirectConnectorData` 的方法 `fn peer_satisfied(&self, dst_peer_id: PeerId) -> bool`。**推荐后者**（能直接用 `self.peer_manager`）。

   替换点：

   | 位置 | 原来 | 改成 |
   | --- | --- | --- |
   | `collect_peers_need_task` :349 | `!data.peer_manager.has_directly_connected_conn(route.peer_id)` | `!data.peer_satisfied(route.peer_id)` |
   | `try_direct_connect` :413 | `if self.peer_manager.has_directly_connected_conn(dst_peer_id) { return Ok(()); }` | `if self.peer_satisfied(dst_peer_id) { return Ok(()); }` |
   | `try_direct_connect_with_ip_list` :468 | 同上 | 同上 |
   | `try_connect_to_url` :599 | `if self.peer_manager.has_directly_connected_conn(dst_peer_id) { return Ok(()); }` | `if self.peer_satisfied(dst_peer_id) { return Ok(()); }` |
   | `try_connect_to_url` :603 | `if result.is_ok() \|\| self.peer_manager.has_directly_connected_conn(dst_peer_id) {` | `if result.is_ok() \|\| self.peer_satisfied(dst_peer_id) {` |

   > `try_connect_to_url` 的 `self` 是 `Arc<Self>` → `self.peer_satisfied(...)` 可直接调（方法接收 `&self`）。
   >
   > `collect_peers_need_task` 拿到的是 `DirectConnectorData`（不是 Arc<Self>）→ 用 `data.peer_satisfied(...)`；若 `peer_satisfied` 定义在 `impl DirectConnectorData` 上则天然可用。

5. **listener 族过滤**。在 `try_direct_connect_with_ip_list`（:419-473）构造 `available_listeners` 之后、开始分组尝试之前加：

   ```rust
   let prefer = self.peer_manager.direct_conn_family();
   let available_listeners = family_filter::filter_listeners_by_prefer_family(
       available_listeners,
       prefer,
       |listener| listener.url.as_str(),
   );
   ```

   > proto 的 `common::Url` 字段名是 `url`（`easytier-proto/proto/common.proto:224`）→ `listener.url.as_str()`。
   >
   > `available_listeners` 当前的类型是 `Vec<Url>`（proto Url），按实际代码调整闭包参数类型。

6. **`IpVersion` 限定**（`connect_ordinary` :650-667）：

   ```rust
   let ip_version = family_filter::prefer_ip_version(self.peer_manager.direct_conn_family());
   let remote_addr = resolve_remote_addr(
       self.peer_manager.as_ref(),
       self.host.as_ref(),
       self.dns.as_ref(),
       &normalized,
       default_port,
       self.options.socket_context(transport, ip_version),
   )
   .await?;
   ```

   > `connect_ordinary` 里的 `self` 是 `&self`（`DirectConnectorData`）→ `self.peer_manager.direct_conn_family()` 可用。

7. 跑测试与编译：

   ```bash
   cargo test -p easytier-core --lib connectivity::direct
   cargo check -p easytier-core --lib
   ```

8. 提交：

   ```bash
   git add easytier-core/src/connectivity/direct
   git commit -m "feat(core): make direct connector family aware"
   ```

---

### Task 5: 对称 NAT 打洞层族感知

**Files:**

- Modify: `easytier-core/src/connectivity/hole_punch/peer_adapters.rs`（`UdpHolePunchPeerSource::candidates()` :40-64；`TcpHolePunchPeerSource::candidates()` 约 :150-160）

**Interfaces:**

- Consumes: Task 2 的 `PeerMap::has_conn_of_family`；Task 1 的 flag。

**Steps:**

1. **先写失败测试**。`peer_adapters.rs` 里的候选生成依赖 `PeerMap`，直接构造代价高。把判定抽成纯函数再测：

   在 `easytier-core/src/connectivity/hole_punch/peer_adapters.rs` 内加（与 `candidates()` 同文件）：

   ```rust
   /// 该 peer 是否已满足「不需要再打洞」的条件。
   /// - Auto：保持上游语义（已有任意连接就不打洞）。
   /// - prefer_ipv4 / prefer_ipv6：已有偏好族连接才不打洞。
   ///   例外：UDP 对称打洞只能产出 IPv4 路径，prefer_ipv6 下保持原样。
   fn has_satisfied_punch_condition(
       peer_map: &PeerMap,
       peer_id: PeerId,
       prefer: ConnFamilyPref,
       udp_only_ipv4: bool,
   ) -> bool {
       match prefer.preferred_family() {
           Some(family) if !(udp_only_ipv4 && prefer == ConnFamilyPref::PreferIpv6) => {
               peer_map.has_conn_of_family(peer_id, family)
           }
           _ => peer_map.has_peer(peer_id),
       }
   }
   ```

   测试（放在同文件 `#[cfg(test)] mod tests`）：

   ```rust
   #[test]
   fn auto_uses_any_connection() {
       // 无 peer 时两种判定都为 false；Auto 走 has_peer 分支
       let prefer = ConnFamilyPref::Auto;
       assert!(!has_satisfied_punch_condition(&empty_peer_map(), PeerId::new(0), prefer, true));
   }
   ```

   > `PeerMap` 构造需要 `Arc<CorePeerContext>`；若构造代价过高，则把函数签名改成接收闭包
   > `has_conn: impl Fn(PeerId) -> bool` 与 `has_any: impl Fn(PeerId) -> bool`，测试用闭包注入，
   > 生产代码传 `|id| peer_map.has_conn_of_family(id, family)` / `|id| peer_map.has_peer(id)`。**推荐闭包版**，可测性最好。

2. 跑测试 → 失败 ✓

3. 改 `UdpHolePunchPeerSource::candidates()`（:59）：

   ```rust
   has_direct_connection: has_satisfied_punch_condition(
       peer_map,
       route.peer_id,
       prefer,
       true, // UDP 打洞只能产出 IPv4
   ),
   ```

   其中 `prefer` 从 `self.get_peer_manager().direct_conn_family()` 或等价的实时来源取（`UdpHolePunchPeerSource` 能拿到 `PeerManagerCore` 的引用 → 用它的 `direct_conn_family()`）。

4. 改 `TcpHolePunchPeerSource::candidates()`（:156 附近）：同样替换 `peer_map.has_peer(route.peer_id)`，但 `udp_only_ipv4 = false`（TCP 两族都支持）：

   ```rust
   has_direct_connection: has_satisfied_punch_condition(
       peer_map,
       route.peer_id,
       prefer,
       false,
   ),
   ```

5. 消费点（`udp/task.rs:63`、`tcp.rs:774`）**不改** —— 它们只看 `candidate.has_direct_connection` 的真假。

6. 跑测试与编译：

   ```bash
   cargo test -p easytier-core --lib hole_punch
   cargo check -p easytier-core --lib
   ```

7. 提交：

   ```bash
   git add easytier-core/src/connectivity/hole_punch
   git commit -m "feat(core): relax hole punch skip condition by preferred family"
   ```

---

### Task 6: 偏好族重试预算与冷却

**Files:**

- Create: `easytier-core/src/connectivity/direct/family_budget.rs`
- Modify: `easytier-core/src/connectivity/direct/mod.rs`（`DirectConnectorData` :163-178；`collect_peers_need_task` :322-354；`try_direct_connect` :370-417）

**Interfaces:**

- Produces: `family_budget::{FamilyBudgetState, FAMILY_CONNECT_ROUNDS_PER_WINDOW, FAMILY_CONNECT_WINDOW_COOLDOWN, FAMILY_CONNECT_MAX_WINDOWS}`。
- Consumes: Task 4 的 `peer_satisfied` / `direct_conn_family`。

**Steps:**

1. **先写失败测试**（写在 `family_budget.rs` 同文件）：

   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;

       #[test]
       fn window_stops_after_three_rounds() {
           let mut state = FamilyBudgetState::default();
           let t0 = Instant::now();
           assert!(state.allow_round(t0));
           assert!(state.allow_round(t0));
           assert!(state.allow_round(t0));
           // 第四轮进入冷却
           assert!(!state.allow_round(t0));
       }

       #[test]
       fn cooldown_expires_after_window() {
           let mut state = FamilyBudgetState::default();
           let t0 = Instant::now();
           for _ in 0..3 {
               state.record_round_failure(t0);
           }
           assert!(!state.allow_round(t0));
           let later = t0 + FAMILY_CONNECT_WINDOW_COOLDOWN + Duration::from_secs(1);
           assert!(state.allow_round(later));
       }

       #[test]
       fn stops_after_five_windows() {
           let mut state = FamilyBudgetState::default();
           let mut now = Instant::now();
           for window in 0..FAMILY_CONNECT_MAX_WINDOWS {
               for _ in 0..FAMILY_CONNECT_ROUNDS_PER_WINDOW {
                   assert!(state.allow_round(now), "window {window}");
                   state.record_round_failure(now);
               }
               now += FAMILY_CONNECT_WINDOW_COOLDOWN + Duration::from_secs(1);
           }
           assert!(!state.allow_round(now));
       }

       #[test]
       fn success_clears_budget() {
           let mut state = FamilyBudgetState::default();
           let t0 = Instant::now();
           state.record_round_failure(t0);
           state.record_success();
           assert!(state.allow_round(t0));
       }
   }
   ```

2. **实现 `family_budget.rs`**：

   ```rust
   use std::time::{Duration, Instant};

   /// 一个窗口内的尝试轮数。
   pub const FAMILY_CONNECT_ROUNDS_PER_WINDOW: u32 = 3;
   /// 窗口失败后的冷却时长。
   pub const FAMILY_CONNECT_WINDOW_COOLDOWN: Duration = Duration::from_secs(120);
   /// 连续失败多少个窗口后放弃。
   pub const FAMILY_CONNECT_MAX_WINDOWS: u32 = 5;

   /// 单个 peer 的偏好族连接尝试预算。
   #[derive(Debug, Default, Clone)]
   pub struct FamilyBudgetState {
       /// 已完成的窗口数。
       pub windows: u32,
       /// 当前窗口内已用的轮数。
       pub rounds: u32,
       /// 冷却到期时间。
       pub cooldown_until: Option<Instant>,
       /// 是否已放弃（达到窗口上限）。
       pub exhausted: bool,
   }

   impl FamilyBudgetState {
       /// 本轮是否允许尝试；冷却中或已放弃返回 false。
       pub fn allow_round(&mut self, now: Instant) -> bool {
           if self.exhausted {
               return false;
           }
           if let Some(until) = self.cooldown_until {
               if now < until {
                   return false;
               }
               // 冷却结束，进入下一个窗口
               self.cooldown_until = None;
               self.rounds = 0;
           }
           self.rounds < FAMILY_CONNECT_ROUNDS_PER_WINDOW
       }

       /// 一轮失败。
       pub fn record_round_failure(&mut self, now: Instant) {
           self.rounds += 1;
           if self.rounds >= FAMILY_CONNECT_ROUNDS_PER_WINDOW {
               self.windows += 1;
               self.rounds = 0;
               if self.windows >= FAMILY_CONNECT_MAX_WINDOWS {
                   self.exhausted = true;
               } else {
                   self.cooldown_until = Some(now + FAMILY_CONNECT_WINDOW_COOLDOWN);
               }
           }
       }

       /// 偏好族连接建立成功 → 清零。
       pub fn record_success(&mut self) {
           self.windows = 0;
           self.rounds = 0;
           self.cooldown_until = None;
           self.exhausted = false;
       }
   }
   ```

3. 在 `DirectConnectorData`（:163-178）里加 per-peer 状态：

   ```rust
       /// 偏好族连接尝试预算（仅偏好非 Auto 时使用）。
       family_budget: Mutex<HashMap<PeerId, FamilyBudgetState>>,
   ```

   `Mutex` 用 `std::sync::Mutex`（持锁期间不 await）。在 `DirectConnectorData` 的构造处初始化为 `Mutex::new(HashMap::new())`。

4. **两个入口都要查**：

   - `collect_peers_need_task`（:322-354）：在已有的 `peer_blacklist` 过滤之后加一条 —— 偏好非 Auto 且该 peer 的预算已耗尽/冷却中 → 不排任务。
   - `try_direct_connect`（:370-417）入口：同样复查；`allow_round` 为 false → 直接返回（不进入退避循环）。

   建议封装成 `DirectConnectorData` 的方法：

   ```rust
       /// 偏好族尝试是否被允许（Auto 恒允许）。
       fn family_attempt_allowed(&self, dst_peer_id: PeerId, now: Instant) -> bool {
           if self.peer_manager.direct_conn_family().preferred_family().is_none() {
               return true;
           }
           let mut budgets = self.family_budget.lock().unwrap();
           budgets
               .entry(dst_peer_id)
               .or_default()
               .allow_round(now)
       }

       /// 一轮尝试失败。
       fn family_round_failed(&self, dst_peer_id: PeerId, now: Instant) {
           if self.peer_manager.direct_conn_family().preferred_family().is_none() {
               return;
           }
           let mut budgets = self.family_budget.lock().unwrap();
           budgets
               .entry(dst_peer_id)
               .or_default()
               .record_round_failure(now);
       }

       /// 偏好族连接成功 → 清零。
       fn family_attempt_succeeded(&self, dst_peer_id: PeerId) {
           if self.peer_manager.direct_conn_family().preferred_family().is_none() {
               return;
           }
           let mut budgets = self.family_budget.lock().unwrap();
           if let Some(state) = budgets.get_mut(&dst_peer_id) {
               state.record_success();
           }
       }
   ```

   - `try_direct_connect` 的退避循环里，每轮开始前调 `family_attempt_allowed`，失败后调 `family_round_failed`；一旦 `peer_satisfied(dst_peer_id)` 为真（偏好族连上了）→ `family_attempt_succeeded` 并返回 `Ok(())`。
   - `try_direct_connect` 的外层循环轮次上限：偏好非 Auto 时最多跑 `FAMILY_CONNECT_ROUNDS_PER_WINDOW` 轮（3 轮），`Auto` 保持上游的无限循环。

5. 日志：冷却与放弃要能看到（验收标准 #3 要求）：

   ```rust
   tracing::warn!(
       ?dst_peer_id,
       "direct connect family preference cooling down"
   );
   ```
   放弃时 `tracing::warn!(?dst_peer_id, "direct connect family preference gave up")`。

6. 跑测试与编译：

   ```bash
   cargo test -p easytier-core --lib family_budget
   cargo check -p easytier-core --lib
   ```

7. 提交：

   ```bash
   git add easytier-core/src/connectivity/direct
   git commit -m "feat(core): bound preferred family direct connect attempts"
   ```

---

### Task 7: 前端三态控件

**Files:**

- Modify: `easytier-web/frontend-lib/src/components/Config.vue`（高级设置 Panel :371；`bool_flags` :128）
- Modify: `easytier-web/frontend-lib/src/locales/cn.yaml`、`en.yaml`
- Create: `easytier-web/frontend-lib/tests/direct-conn-family.spec.ts`
- (Task 1 已生成) `easytier-web/frontend-lib/src/generated/proto/**`

**Interfaces:**

- Consumes: `NetworkConfig.direct_conn_family`（Task 1 生成）。
- Produces: 网页配置页的三态选择（`auto` / `prefer_ipv4` / `prefer_ipv6`）。

**Steps:**

1. **先写失败测试**（新建 `easytier-web/frontend-lib/tests/direct-conn-family.spec.ts`）。照抄 `tests/config-ui.spec.ts` 的骨架（`mount` + `vi.mock('vue-i18n')`）：

   ```ts
   import { mount } from '@vue/test-utils'
   import { describe, expect, it, vi } from 'vitest'
   import Config from '../src/components/Config.vue'
   import { NetworkConfig as NetworkConfigPb } from '../src/generated/proto/api_manage'
   import { DEFAULT_NETWORK_CONFIG, type NetworkConfig } from '../src/types/network'

   vi.mock('vue-i18n', () => ({
     useI18n: () => ({
       t: (key: string, values?: unknown[]) => (values ? `${key}:${values.join(',')}` : key),
     }),
   }))

   const FIELD = 'direct_conn_family'

   it('the management API carries the new field', () => {
     expect(NetworkConfigPb.fields.some((f) => f.name === FIELD)).toBe(true)
   })

   it('renders three options and defaults to auto', () => {
     const wrapper = mount(Config, { props: { modelValue: { ...DEFAULT_NETWORK_CONFIG } } })
     const buttons = wrapper.findAll('#direct_conn_family button')
     expect(buttons.length).toBe(3)
     expect(wrapper.find('#direct_conn_family').text()).toContain('auto')
   })
   ```

   > `Config.vue` 的 props 形状与 `config-ui.spec.ts` 里的挂载方式保持一致（照抄该文件里已有的 `mount(Config, ...)` 调用），不要凭空造 props。

2. 跑测试 → 失败（`#direct_conn_family` 不存在）✓

3. **实现控件**（`Config.vue`）：

   - 在高级设置 Panel（:371）内、`bool_flags` 循环（:379）之后加一行：

     ```vue
     <div class="basis-[20rem] flex items-center" v-if="apiCarries('direct_conn_family')">
       <label :for="FIELD" class="mr-2">{{ t('direct_conn_family') }}</label>
       <SelectButton
         :id="FIELD"
         v-model="directConnFamilyValue"
         :options="directConnFamilyOptions"
         option-label="label"
         option-value="value"
         :allow-empty="false"
       />
     </div>
     ```

   - `script` 里加：

     ```ts
     const directConnFamilyOptions = [
       { label: 'auto', value: 'auto' },
       { label: 'prefer_ipv4', value: 'prefer_ipv4' },
       { label: 'prefer_ipv6', value: 'prefer_ipv6' },
     ]
     ```

   - 值的读写（与 `Config.vue` 现有 model 同步机制一致 —— 照抄它对其它字段的读写方式，如 `ipv6_public_addr_auto`）：
     - 读：`config.value.direct_conn_family`（或组件现有的等效访问），为空 / 不在三个合法值内 → `'auto'`。
     - 写：选择变化时写回 `direct_conn_family`。
   - **默认值不硬编码**：用 `flag.meta.default`（proto 注解里的 `"auto"`）作为回退值 —— 在 `network.ts` 里用 `defaultFlagValue('direct_conn_family')` 或直接读 `FlagsPb` 的注解。

   > `Config.vue` 里 `apiCarries` 若不存在，就用 `NetworkConfigPb.fields.some(...)`（`network.ts:146` 已有 `apiCarries` 实现，可直接 import）。

4. **i18n**（`easytier-web/frontend-lib/src/locales/cn.yaml` / `en.yaml`）：

   ```yaml
   direct_conn_family: "直连地址族偏好"
   direct_conn_family_help: "优先用 IPv4 或 IPv6 建立直连；auto 为默认"
   ```
   ```yaml
   direct_conn_family: "Direct connection family"
   direct_conn_family_help: "Prefer IPv4 or IPv6 for direct connections; auto is the default"
   ```

5. **前端测试**（`tests/direct-conn-family.spec.ts`）覆盖：

   - 三个选项都在；
   - 当前值回显正确（`prefer_ipv4` → 选中第二个）；
   - 空值 / 未知值 → 回落 `auto`；
   - 选择后 `NetworkConfig.direct_conn_family` 被写入；
   - `toBackendNetworkConfig` 带上该字段。

6. 跑测试与 lint：

   ```bash
   pnpm --dir easytier-web/frontend-lib test -- tests/direct-conn-family.spec.ts
   pnpm --dir easytier-web/frontend-lib lint
   ```

7. 提交（**不要**带上 `auto-imports.d.ts` / `typed-router.d.ts` / `pnpm-lock.yaml` 的意外改动）：

   ```bash
   git checkout -- easytier-gui/src/auto-imports.d.ts easytier-gui/src/typed-router.d.ts pnpm-lock.yaml 2>/dev/null || true
   git add easytier-web/frontend-lib/src/components/Config.vue easytier-web/frontend-lib/src/locales easytier-web/frontend-lib/tests/direct-conn-family.spec.ts
   git commit -m "feat(web): add direct connection family selector to config form"
   ```

---

### Task 8: 全量验证与文档

**Files:**

- Modify: `docs/superpowers/runbooks/android-exit-node-openclash.md`（若需补充族偏好用法）或新增 `docs/superpowers/runbooks/direct-conn-family.md`

**Steps:**

1. Rust 全量检查：

   ```bash
   cargo fmt --all
   cargo clippy -p easytier-core --lib -- -D warnings
   cargo test -p easytier-core --lib 2>&1 | tail -30
   cargo test -p easytier-proto 2>&1 | tail -10
   ```

   与 Task 0 记录的基线测试数对比，确认**没有新失败**（新增测试数应更多）。

2. 前端全量：

   ```bash
   pnpm --dir easytier-web/frontend-lib test
   pnpm --dir easytier-web/frontend-lib lint
   pnpm --dir easytier-web/frontend-lib build
   git checkout -- easytier-gui/src/auto-imports.d.ts easytier-gui/src/typed-router.d.ts pnpm-lock.yaml 2>/dev/null || true
   ```

3. 写 runbook（面向使用者）：

   ```markdown
   # 直连族偏好（direct_conn_family）

   ## 用法
   - CLI：`--direct-conn-family prefer_ipv4`
   - TOML：`[flags]` 段 `direct_conn_family = "prefer_ipv4"`
   - 网页配置页：高级设置 → 直连地址族偏好

   ## 语义
   - auto（默认）：与上游一致。
   - prefer_ipv4：优先用 IPv4 建立直连并承载流量；IPv6 连接保留作回退。
   - prefer_ipv6：反向。

   ## 行为
   - 运行期修改最多 5 秒生效，不需要重启实例。
   - 偏好族打不通时：3 轮（约 15 秒）→ 冷却 120 秒 → 5 个窗口（约 9 分钟）后放弃，
     期间另一族连接保持可用。
   - 只影响直连（打洞）路径选择，不影响中继、IPv6 监听与其它 flag。
   ```

4. 提交：

   ```bash
   git add docs/superpowers/runbooks
   git commit -m "docs: add direct connection family runbook"
   ```

5. 推分支（如果需要触发 CI）：

   ```bash
   git push --force-with-lease origin releases/mine
   ```

6. **最终验收自查**（对照 spec §8）：

   | # | 验收项 | 怎么查 |
   | --- | --- | --- |
   | 1 | 默认配置与上游一致 | `conn_sort_key` 的 Auto 测试 + 五处判定在 Auto 下走原分支 |
   | 2 | prefer_ipv4 时默认连接是 V4 | 排序键测试 + 手工看日志 |
   | 3 | 偏好族不通时收敛 | budget 测试 + 日志里的 cooling down / gave up |
   | 4 | 打通后计数清零 | `record_success` 测试 |
   | 5 | CLI/TOML/网页三处一致 | TOML 测试 + 前端测试 + `--direct-conn-family` 出现在 `--help` |
   | 6 | IPv6 监听/中继不受影响 | 代码审查：本次没碰相关路径 |

---

## 任务依赖图

```
Task 0 (环境)
  └─ Task 1 (proto + 配置面)
       └─ Task 2 (族判定原语) ──┬─ Task 3 (选路)
                                ├─ Task 4 (发起层) ── Task 6 (重试预算)
                                └─ Task 5 (对称打洞)
  Task 7 (前端) 依赖 Task 1 的生成物
  Task 8 (全量验证) 依赖 3/4/5/6/7
```

> Task 3、Task 4、Task 5 之间互不依赖，可并行；Task 6 依赖 Task 4 的 `peer_satisfied` 语义；Task 7 可与 3/4/5/6 并行。
