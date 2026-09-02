# Android 全流量退出节点 + 自定义 VPN DNS 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `releases/mine` 分支上为 Android GUI 增加"自定义 VPN DNS"能力（localStorage 持久化 + 底部菜单对话框 + DNS 优先级），并建立补丁维护与退出侧运维流程。

**Architecture:** 全部代码改动限于 `easytier-gui/`（Rust、proto、`easytier-web/frontend-lib` 零改动）。DNS 选择优先级在 `mobile_vpn.ts` 的 reconcile 中生效：自定义 IPv4 > magic DNS 固定地址 > 不设 DNS；`dnsChanged` 既有逻辑自动触发 VPN 重启。UI 入口是 `index.vue` 底部菜单中仅 Android 可见的一项，仿照既有 config-server Dialog 模式。

**Tech Stack:** Vue 3 `<script setup>` + PrimeVue 4（组件自动导入）、TypeScript、vitest 2（node 环境，无 jsdom——测试中用 `vi.stubGlobal` 桩掉 `localStorage`）、pnpm 9（经 `corepack` 调用，本机 PATH 无 pnpm）、GitHub Actions（fork 的 `mobile.yml`，push 到 `releases/**` 自动出 4-ABI APK）。

**Spec:** `docs/superpowers/specs/2026-09-02-android-exit-node-custom-vpn-dns-design.md`

**工作目录:** 所有相对路径基于仓库根 `D:\Github\EasyTier`，当前分支必须是 `releases/mine`（`git branch --show-current` 验证）。

**文件结构（改动面）:**

| 文件 | 操作 | 职责 |
|---|---|---|
| `easytier-gui/src/composables/config.ts` | 修改 | 新增 `isValidIpv4` / `saveCustomVpnDns` / `loadCustomVpnDns`（localStorage key `custom_vpn_dns`） |
| `easytier-gui/src/composables/mobile_vpn.ts` | 修改 | reconcile 中 DNS 优先级改为 custom > magic > undefined |
| `easytier-gui/src/composables/mobile_vpn.test.ts` | 修改 | localStorage 桩 + `setConfig` 增加 magic 参数 + 4 个 DNS 选择用例 |
| `easytier-gui/src/pages/index.vue` | 修改 | Android-only 菜单项 + 自定义 VPN DNS 对话框 |
| `docs/superpowers/runbooks/android-exit-node-openclash.md` | 新建 | 退出侧运维文档（可复制命令） |

不修改：`package.json`（用既有脚本 `test:mobile-vpn`）、locale 文件（新 UI 文字用中文字面量）、任何 Rust/proto/frontend-lib 文件。

---

### Task 1: 环境准备（pnpm 安装）

**Files:** 无代码改动。

- [ ] **Step 1: 确认分支**

Run: `git branch --show-current`
Expected: `releases/mine`

- [ ] **Step 2: 用 corepack 安装 workspace 依赖**

本机 PATH 没有 `pnpm`，但 `corepack` 可用且 `easytier-gui/package.json` 的 `packageManager` 字段锁定 pnpm@9.12.1（首次会联网下载）。

Run（仓库根）: `corepack pnpm install`
Expected: 以 `Done in ...` 结束，出现 `node_modules/`；若报 pnpm 下载失败，重试一次（网络问题）。

---

### Task 2: DNS 优先级（测试先行：config.ts + mobile_vpn.ts + 测试）

**Files:**
- Modify: `easytier-gui/src/composables/mobile_vpn.test.ts`
- Modify: `easytier-gui/src/composables/config.ts`
- Modify: `easytier-gui/src/composables/mobile_vpn.ts:363`

- [ ] **Step 1: 写失败测试**

编辑 `easytier-gui/src/composables/mobile_vpn.test.ts`：

1a. 在文件顶部 `const mocks = vi.hoisted(...)` 语句**之前**加：

```ts
const vpnDnsStorage = new Map<string, string>()
```

1b. 把现有 `setConfig`（约 line 59）整体替换为（新增第三个参数，默认值保持既有调用不变）：

```ts
function setConfig(instanceId: string, noTun = false, enableMagicDns = false) {
  mocks.configs.set(instanceId, {
    no_tun: noTun,
    dhcp: false,
    enable_magic_dns: enableMagicDns,
    routes: [],
  })
}
```

1c. 在现有 `beforeEach(() => { ... })` 块内末尾（`mocks.stopVpn.mockClear()` 之后、闭合 `})` 之前）加：

```ts
  vpnDnsStorage.clear()
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => vpnDnsStorage.get(key) ?? null,
    setItem: (key: string, value: string) => {
      vpnDnsStorage.set(key, value)
    },
    removeItem: (key: string) => {
      vpnDnsStorage.delete(key)
    },
  })
```

1d. 在文件末尾追加新 describe 块：

```ts
describe('mobile VPN dns selection', () => {
  it('passes custom dns to the vpn service when stored value is a valid IPv4', async () => {
    vpnDnsStorage.set('custom_vpn_dns', '10.7.7.7')
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({ dns: '10.7.7.7' }))
  })

  it('prefers custom dns over the magic dns address', async () => {
    vpnDnsStorage.set('custom_vpn_dns', '10.7.7.7')
    setConfig('A', false, true)
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({ dns: '10.7.7.7' }))
  })

  it('ignores an invalid stored dns and falls back to magic dns', async () => {
    vpnDnsStorage.set('custom_vpn_dns', 'not-an-ip')
    setConfig('A', false, true)
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({ dns: '100.100.100.101' }))
  })

  it('keeps stock dns behavior when no custom dns is stored', async () => {
    setConfig('A', false, true)
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({ dns: '100.100.100.101' }))
  })
})
```

- [ ] **Step 2: 运行测试确认失败**

Run: `corepack pnpm --dir easytier-gui test:mobile-vpn`
Expected: `Tests 2 failed | 9 passed`——前两个新用例失败（实际 `dns: undefined` ≠ `'10.7.7.7'`）；后两个新用例和 7 个既有用例通过（fallback 行为本来就等于 stock 行为，属预期）。若失败数不是 2，停下排查。

- [ ] **Step 3: 实现 config.ts 助手**

在 `easytier-gui/src/composables/config.ts` 末尾（`loadLastNetworkInstanceId` 之后）追加，风格与现有注释一致：

```ts
const CUSTOM_VPN_DNS_KEY = 'custom_vpn_dns'

/**
 * 校验字符串是否为合法 IPv4 地址（点分四段，每段 0-255）
 * @param value 待校验字符串
 */
export function isValidIpv4(value: string): boolean {
    const parts = value.split('.')
    if (parts.length !== 4) {
        return false
    }
    return parts.every(part => /^\d{1,3}$/.test(part) && Number(part) <= 255)
}

/**
 * 保存自定义 VPN DNS（仅 Android 生效）。传 null 或空串表示清除
 * @param dns IPv4 地址字符串，或 null 表示不启用
 */
export function saveCustomVpnDns(dns: string | null) {
    if (dns === null || dns.trim().length === 0) {
        localStorage.removeItem(CUSTOM_VPN_DNS_KEY)
        return
    }
    localStorage.setItem(CUSTOM_VPN_DNS_KEY, dns.trim())
}

/**
 * 加载自定义 VPN DNS
 * @returns 自定义 DNS 字符串，未设置则为 null
 */
export function loadCustomVpnDns(): string | null {
    return localStorage.getItem(CUSTOM_VPN_DNS_KEY)
}
```

（注意：该文件现有缩进是 4 空格，照抄上面的缩进。）

- [ ] **Step 4: 实现 mobile_vpn.ts DNS 优先级**

4a. 在 `easytier-gui/src/composables/mobile_vpn.ts` 顶部 import 区（`import { collectNetworkInfo, ... } from './backend'` 之后）加：

```ts
import { isValidIpv4, loadCustomVpnDns } from './config'
```

4b. 把现有 line 363：

```ts
  const dns = config.enable_magic_dns ? '100.100.100.101' : undefined
```

替换为：

```ts
  const customDns = loadCustomVpnDns()
  const dns = (customDns !== null && isValidIpv4(customDns))
    ? customDns
    : (config.enable_magic_dns ? '100.100.100.101' : undefined)
```

其余不动（`dnsChanged` 比较与 `doStartVpn` 透传复用现有逻辑）。

- [ ] **Step 5: 运行测试确认全部通过**

Run: `corepack pnpm --dir easytier-gui test:mobile-vpn`
Expected: `Test Files 1 passed`、`Tests 11 passed`。

- [ ] **Step 6: Commit**

```powershell
git add easytier-gui/src/composables/config.ts easytier-gui/src/composables/mobile_vpn.ts easytier-gui/src/composables/mobile_vpn.test.ts
git commit -m "feat(gui): support custom VPN DNS server on Android"
```

---

### Task 3: Android 专属设置入口（index.vue 菜单项 + 对话框）

**Files:**
- Modify: `easytier-gui/src/pages/index.vue`

无组件测试基础设施，本任务验证靠 lint + 构建期类型检查（Task 4）；人工验收在 Task 6。

- [ ] **Step 1: 扩展 import（line 17）**

把：

```ts
import { saveLastNetworkInstanceId, loadLastNetworkInstanceId } from '~/composables/config'
```

改为：

```ts
import { isValidIpv4, loadCustomVpnDns, loadLastNetworkInstanceId, saveCustomVpnDns, saveLastNetworkInstanceId } from '~/composables/config'
```

- [ ] **Step 2: 新增对话框状态（line 30-31 `configServerConnected` 声明之后）**

```ts
const customVpnDnsDialogVisible = ref(false)
const editingCustomVpnDns = ref('')
```

- [ ] **Step 3: 新增打开/保存函数（`async function openConfigServerDialog() {...}` 定义之后，约 line 394）**

```ts
function openCustomVpnDnsDialog() {
  editingCustomVpnDns.value = loadCustomVpnDns() ?? ''
  customVpnDnsDialogVisible.value = true
}

async function onCustomVpnDnsSave() {
  const value = editingCustomVpnDns.value.trim()
  if (value.length && !isValidIpv4(value)) {
    toast.add({ severity: 'error', summary: t('error'), detail: '请输入合法的 IPv4 地址，或留空恢复默认', life: 5000 })
    return
  }
  saveCustomVpnDns(value.length ? value : null)
  customVpnDnsDialogVisible.value = false
  try {
    await syncMobileVpnService()
    toast.add({ severity: 'success', summary: t('web.common.success'), detail: 'VPN DNS 已更新，正在重启 VPN 生效', life: 3000 })
  }
  catch (e: any) {
    toast.add({ severity: 'error', summary: t('error'), detail: e, life: 10000 })
    console.error('Error syncing vpn service after dns change', e)
  }
}
```

- [ ] **Step 4: 新增菜单项（`setting_menu_items` 数组内，config-server 项之后、`key: 'logging_menu'` 项之前）**

```ts
  {
    label: () => '自定义 VPN DNS',
    icon: 'pi pi-cog',
    command: openCustomVpnDnsDialog,
    visible: () => type() === 'android',
  },
```

（`visible` 方向注意：这里是 `=== 'android'`，与既有 mode 项的 `!== 'android'` 相反——本功能仅 Android 有意义。）

- [ ] **Step 5: 新增对话框模板（config-server `</Dialog>` 之后，约 line 475）**

```html
    <Dialog v-model:visible="customVpnDnsDialogVisible" modal header="自定义 VPN DNS" :style="{ width: '70vw' }">
      <div class="flex flex-col gap-3">
        <label for="custom-vpn-dns">DNS 服务器 IPv4 地址</label>
        <InputText id="custom-vpn-dns" v-model="editingCustomVpnDns" placeholder="例如 192.168.123.2，留空恢复默认" />
        <small class="p-text-secondary">仅 Android 生效：VPN 的 DNS 查询将发往该地址（配合退出节点可实现经隧道解析）；留空则沿用 magic DNS 行为。</small>
      </div>
      <template #footer>
        <Button :label="t('web.common.cancel')" icon="pi pi-times" @click="customVpnDnsDialogVisible = false" text />
        <Button label="保存" icon="pi pi-save" @click="onCustomVpnDnsSave" autofocus />
      </template>
    </Dialog>
```

（`Dialog`/`InputText`/`Button` 由 PrimeVueResolver 自动导入，无需 import；`openCustomVpnDnsDialog` 函数声明提升，可安全前向引用——既有 `openConfigServerDialog` 同款模式。）

- [ ] **Step 6: Commit**

```powershell
git add easytier-gui/src/pages/index.vue
git commit -m "feat(gui): add android-only custom vpn dns dialog"
```

---

### Task 4: Lint 与改动面核查

**Files:** 无新增改动（仅可能产生 lint --fix 修复）。

- [ ] **Step 1: 对 4 个改动文件跑 eslint**

Run: `corepack pnpm --dir easytier-gui exec eslint src/composables/config.ts src/composables/mobile_vpn.ts src/composables/mobile_vpn.test.ts src/pages/index.vue`
Expected: 退出码 0、无输出。若有可自动修复项：`... eslint --fix <同上文件>` 后重跑确认干净，并把修复计入 Task 3 的文件重新 commit（`git add` 相应文件，`git commit -m "style: lint fixes for custom vpn dns patch"`）。

- [ ] **Step 2: 全量单测复跑**

Run: `corepack pnpm --dir easytier-gui test:mobile-vpn`
Expected: `Tests 11 passed`。

- [ ] **Step 3: 核查改动面未越界**

Run: `git diff --name-only main..HEAD`
Expected: 仅 `docs/superpowers/specs/...` 与上表 4 个 `easytier-gui/` 文件（Task 5 前无 runbook）。任何 `easytier/`、`easytier-core/`、`easytier-web/`、`*.proto` 路径出现即为违规，必须回退对应改动。

---

### Task 5: 退出侧 runbook 文档 + 补丁维护流程固化

**Files:**
- Create: `docs/superpowers/runbooks/android-exit-node-openclash.md`

- [ ] **Step 1: 写 runbook**

创建 `docs/superpowers/runbooks/android-exit-node-openclash.md`，内容以 spec §4 为骨架并展开成可复制命令，必须包含以下小节与要点（占位符统一用 `<EASYTIER_NET>`=easytier 虚拟网段如 `10.144.0.0/16`、`<EXIT_IF>`=exit host 出口网卡、`<VM_IP>`=OpenWrt VM 的 LAN IP）：

1. 拓扑图（文字版）：手机 VPN(0.0.0.0/0, DNS=`<VM_IP>`) → 隧道 → exit host → 策略路由 → OpenWrt VM(OpenClash) → Clash 规则出口。
2. exit host 内核与 NAT：`sysctl -w net.ipv4.ip_forward=1`；FORWARD 放行；`iptables -t nat -A POSTROUTING -s <EASYTIER_NET> -o <EXIT_IF> -j MASQUERADE`。
3. 策略路由（仅 VPN 流量进 VM，host 自身默认路由不变，天然防环）：`ip rule add from <EASYTIER_NET> table 100` + `ip route add default via <VM_IP> table 100`。
4. OpenClash 侧：开启 DNS 并监听 LAN（`53`）；fake-ip 模式下转发流量走 PREROUTING 透明代理，无需在 host 上做 53 DNAT。
5. 手机侧（stock GUI + 本补丁）：`exit_nodes` = exit 节点虚拟 IP；高级设置手动路由加 `0.0.0.0/0`；exit 节点 GUI 勾选 `enable_exit_node`；底部菜单"自定义 VPN DNS" = `<VM_IP>`。
6. 验证命令：`dig @<VM_IP> <域名>`（host 直测解析器）、手机浏览器开 `https://ip.sb` 比对 Clash 面板命中。
7. 故障排查：首查慢→确认 host 默认网关未指向 VM；DNS 通但流量不出→查 FORWARD/ip_forward；`tcpdump -i any port 53 -n` 观察查询去向。
8. 附录（本期不做）：`.et.net` 经 mihomo 解析——magic DNS 仅 RPC `127.0.0.1:49813`，候选为 easytier `[dns_server]` UDP zone，单独验证。

- [ ] **Step 2: 提交补丁维护记录并 commit**

```powershell
git add docs/superpowers/runbooks/android-exit-node-openclash.md
git commit -m "docs: add exit-node OpenClash runbook"
```

- [ ] **Step 3: 配置 upstream remote**

Run: `git remote add upstream https://github.com/EasyTier/EasyTier`（若已存在则跳过）
Run: `git remote -v`
Expected: 同时存在 `origin`（Fitmtfk/EasyTier）与 `upstream`（EasyTier/EasyTier）。

- [ ] **Step 4: 导出补丁归档（落在仓库外）**

Run: `git format-patch main..releases/mine -o ..\easytier-patches`
Expected: `..\easytier-patches\` 下生成 `0001-*.patch` … 每个提交一个文件。恢复方式（写进 commit 说明或 runbook 末尾亦可）：新克隆上 `git checkout -b mine && git am ..\easytier-patches\*.patch`。

- [ ] **Step 5: 上游 rebase 干跑**

```powershell
git fetch upstream
git rebase upstream/main
```
Expected: `Successfully rebased` 或 `当前分支已是最新`；若冲突，冲突面应仅在 `easytier-gui` 4 文件内——手工合并后 `git rebase --continue`。完成后 `corepack pnpm --dir easytier-gui test:mobile-vpn` 须仍 `11 passed`。

- [ ] **Step 6: 推送 fork 触发 CI（需 GitHub 凭证，失败则报告用户手工推送）**

Run: `git push -u origin releases/mine`
Expected: 推送成功；随后 GitHub → Actions 中 fork 的 mobile workflow 因 `releases/**` 触发，产出 4 个 ABI 的 release APK artifact。若 artifact APK 因签名不可安装，按 spec 退回本地 `corepack pnpm --dir easytier-gui tauri android build --apk` 签名构建。

---

### Task 6: 手工验收（真机 + 退出侧，spec §5 全项）

**Files:** 无代码改动。此任务由用户在真机与退出环境执行，执行者逐项陪同核对并把结果记录在最终汇报里。

- [ ] **Step 1: 退出侧就绪**：按 runbook 在 exit host / OpenWrt VM 执行命令，`dig @<VM_IP> baidu.com` 在 host 上有应答。
- [ ] **Step 2: 手机配置**：安装 CI（或本地）APK；`exit_nodes`=exit vip；手动路由 `0.0.0.0/0`；自定义 VPN DNS=`<VM_IP>`；保存后观察 VPN 自动重启（toast 提示 + DNS 变更触发）。
- [ ] **Step 3: 出口验证**：手机 `https://ip.sb` 显示 Clash 出口 IP 而非手机本地 IP；Clash 面板可见连接命中规则；首查 DNS 无 5s 级超时。
- [ ] **Step 4: 回退验证**：自定义 VPN DNS 留空保存 → 行为回到上游 stock（magic DNS 开启时用 `100.100.100.101`，否则不下发 DNS）。
- [ ] **Step 5: 桌面零影响**：桌面端 GUI 底部菜单无"自定义 VPN DNS"项，其余功能正常。

---

## Self-Review 记录

1. **Spec 覆盖**：spec §3.1→Task 2 Step 3；§3.2→Task 3；§3.3→Task 2 Step 4；§2 补丁机制→Task 5 Steps 3-6；§4 runbook→Task 5 Step 1；§5 验收→Task 4 + Task 6；§6 不做项无对应任务（正确）。无缺口。
2. **占位符扫描**：runbook 任务给出的小节清单即为完整内容要求（运维文档面向用户本人，命令已给全）；代码步骤均含完整代码。无 TBD/TODO。
3. **类型/命名一致性**：`isValidIpv4` / `saveCustomVpnDns` / `loadCustomVpnDns` / key `custom_vpn_dns` / `editingCustomVpnDns` / `customVpnDnsDialogVisible` / `openCustomVpnDnsDialog` / `onCustomVpnDnsSave` 在 Task 2/3 与测试代码中拼写一致；测试桩实现了 `getItem/setItem/removeItem`（`saveCustomVpnDns` 清除路径用到 `removeItem`）。
