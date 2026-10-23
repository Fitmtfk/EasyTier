use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use quanta::Instant;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use url::{Host, Url};

use crate::{
    config::PeerId,
    connectivity::hole_punch::policy::{should_background_p2p_with_peer, should_try_p2p_with_peer},
    connectivity::stun::{StunInfoProvider, StunSocketMapper},
    connectivity::{
        LocalListenerUrls,
        protocol::{
            ClientProtocolUpgrader, ProtocolTransport, protocol_transport, protocol_uses_udp,
        },
        transport::{self, ConnectedTransport, UdpSessionMode},
    },
    foundation::expiring_set::ExpiringSet,
    foundation::task::{PeerTaskLauncher, PeerTaskManager},
    host::dns::DnsResolver,
    peers::{
        PeerConnectionOrigin,
        conn::{family::ConnFamilyPref, peer_conn::PeerConnId},
        foreign_network::ForeignNetworkRpcRegistrar,
        peer_manager::PeerManagerCore,
        peer_rpc::PeerRpcManager,
    },
    process_runtime::ProtectedTcpPortRegistry,
    proto::{
        common::Void,
        peer_rpc::{
            DirectConnectorRpc, DirectConnectorRpcClientFactory,
            DirectConnectorRpcServer as GeneratedDirectConnectorRpcServer, GetIpListRequest,
            GetIpListResponse, SendUdpHolePunchPacketRequest,
        },
        rpc_types::{self, controller::BaseController},
    },
    socket::{
        IpVersion, SocketContext,
        tcp::{TcpBindOptions, TcpSocketPurpose},
        udp::{
            PreferredIpv6Source, UdpBindOptions, VirtualUdpSocket, VirtualUdpSocketFactory,
            send_v4_hole_punch_control_packet, send_v6_hole_punch_control_packet,
        },
    },
    tunnel::Tunnel,
};

use super::manual::{
    ManualConnectorHost, collect_bind_addrs, convert_idn_to_ascii, resolve_remote_addr,
    resolve_url_addrs,
};

mod family_budget;
mod family_filter;
mod udp;

const DIRECT_CONNECTOR_BLACKLIST_TIMEOUT: Duration = Duration::from_secs(300);
const INVALID_SERVICE_BLACKLIST_TIMEOUT: Duration = Duration::from_secs(3600);
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const DIRECT_TASK_LOOP_INTERVAL_MS: u64 = 5000;
const MAX_IPV6_HOLE_PUNCH_CONNECTOR_ADDRS: usize = 16;
const MAX_UDP_HOLE_PUNCH_CONNECTOR_ADDRS: usize = 16;

#[async_trait]
pub trait DirectConnectorHost: ManualConnectorHost {
    async fn collect_ip_addrs(&self, context: &SocketContext) -> GetIpListResponse;

    async fn collect_foreign_ip_addrs(&self, context: &SocketContext) -> GetIpListResponse {
        self.collect_ip_addrs(context).await
    }

    fn mapped_listeners(&self) -> Vec<Url>;

    fn is_local_ip(&self, ip: &IpAddr) -> bool;

    async fn preferred_ipv6_source(
        &self,
        ip: Ipv6Addr,
        context: SocketContext,
    ) -> Option<PreferredIpv6Source>;

    async fn preferred_foreign_ipv6_source(
        &self,
        ip: Ipv6Addr,
        context: SocketContext,
    ) -> Option<PreferredIpv6Source> {
        self.preferred_ipv6_source(ip, context).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectTransport {
    Tcp(TcpSocketPurpose),
    Udp(UdpSessionMode),
}

impl DirectTransport {
    fn from_url(url: &Url) -> anyhow::Result<Self> {
        match protocol_transport(url.scheme()) {
            Some(ProtocolTransport::Tcp) => Ok(Self::Tcp(TcpSocketPurpose::DirectConnect)),
            Some(ProtocolTransport::FakeTcp) => Ok(Self::Tcp(TcpSocketPurpose::FakeTcp)),
            Some(ProtocolTransport::Udp(mode)) => Ok(Self::Udp(mode)),
            None => anyhow::bail!("unsupported direct transport scheme: {}", url.scheme()),
        }
    }

    fn is_udp(self) -> bool {
        matches!(self, Self::Udp(_))
    }

    fn supports_interface_bind(self) -> bool {
        !matches!(self, Self::Tcp(TcpSocketPurpose::FakeTcp))
    }
}

/// 一轮直连的「满足口径」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DirectSatisfaction {
    /// 偏好族已连通即算满足（`auto`：已有直连连接即算满足）。
    Preferred,
    /// 任意族已连通即算满足（兜底 pass 用）。
    AnyFamily,
}

/// 一轮直连的输入：待试的 listener + 满足口径 + 解析时允许的地址族。
struct DirectConnectPass {
    listeners: Vec<Url>,
    satisfaction: DirectSatisfaction,
    ip_version: IpVersion,
}

/// 直连的两轮计划。
struct DirectConnectPlan {
    /// 第一轮：偏好族（`auto` 下是全部 listener）。
    preferred: DirectConnectPass,
    /// 第二轮：另一族兜底（`auto` 下恒为空）。
    other: DirectConnectPass,
}

/// 把 listener 列表拆成「偏好族」与「另一族」两轮。
///
/// - `auto`：全部 listener 留在第一轮，另一轮恒空 → 永不进兜底（与上游逐位一致）。
/// - `prefer_*`：域名 listener 留在第一轮（族要靠 DNS 解析才知道），
///   字面量族 != 偏好族的 listener 进第二轮。
fn plan_direct_connect(listeners: Vec<Url>, prefer: ConnFamilyPref) -> DirectConnectPlan {
    let (preferred_listeners, other_listeners) =
        family_filter::split_listeners_by_prefer_family(listeners, prefer, |listener: &Url| {
            listener.as_str()
        });

    DirectConnectPlan {
        preferred: DirectConnectPass {
            listeners: preferred_listeners,
            satisfaction: DirectSatisfaction::Preferred,
            ip_version: family_filter::prefer_ip_version(prefer),
        },
        other: DirectConnectPass {
            listeners: other_listeners,
            satisfaction: DirectSatisfaction::AnyFamily,
            // 兜底轮连的是另一族地址，解析必须放行两族。
            ip_version: IpVersion::Both,
        },
    }
}

/// 兜底门控：偏好族打不通时，是否允许用另一族 listener 建一条连接。
///
/// 三条同时满足才允许：
/// 1. 偏好不是 `auto`（`auto` 与上游逐位一致，永不兜底）；
/// 2. 另一族 listener 列表非空；
/// 3. 该 peer 当前**没有任何族**的连接（已有连接就不重复建连）。
fn fallback_to_other_family_allowed(
    prefer: ConnFamilyPref,
    other_family_listener_count: usize,
    has_any_family_conn: bool,
) -> bool {
    prefer != ConnFamilyPref::Auto && other_family_listener_count > 0 && !has_any_family_conn
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectConnectorOptions {
    pub default_protocol: String,
    pub enable_ipv6: bool,
    pub allow_public_server: bool,
    pub bind_device: bool,
    pub allow_interface_bind: bool,
    pub tcp_bind: TcpBindOptions,
    pub udp_bind: UdpBindOptions,
    #[serde(skip)]
    pub testing: bool,
}

impl Default for DirectConnectorOptions {
    fn default() -> Self {
        Self {
            default_protocol: "tcp".to_owned(),
            enable_ipv6: true,
            allow_public_server: false,
            bind_device: false,
            allow_interface_bind: true,
            tcp_bind: TcpBindOptions::default(),
            udp_bind: UdpBindOptions::direct_connect(),
            testing: false,
        }
    }
}

impl DirectConnectorOptions {
    fn socket_context(&self, transport: DirectTransport, ip_version: IpVersion) -> SocketContext {
        let context = match transport {
            DirectTransport::Tcp(_) => self.tcp_bind.context.clone(),
            DirectTransport::Udp(_) => self.udp_bind.context.clone(),
        };
        context.with_ip_version(ip_version)
    }
}

#[derive(Debug, Hash, Eq, PartialEq, Clone)]
struct ListenerBlacklistKey(PeerId, String);

struct DirectConnectorData<H>
where
    H: DirectConnectorHost,
{
    peer_manager: Arc<PeerManagerCore>,
    host: Arc<H>,
    protected_tcp_ports: Arc<ProtectedTcpPortRegistry>,
    stun: Arc<dyn StunSocketMapper<<H as VirtualUdpSocketFactory>::Socket>>,
    running_listeners: Arc<dyn LocalListenerUrls>,
    dns: Arc<dyn DnsResolver>,
    protocol:
        Arc<dyn ClientProtocolUpgrader<<H as crate::socket::tcp::VirtualTcpSocketFactory>::Socket>>,
    options: DirectConnectorOptions,
    listener_blacklist: ExpiringSet<ListenerBlacklistKey>,
    peer_blacklist: ExpiringSet<PeerId>,
    /// 每个 peer 的偏好族直连重试预算（spec §4.6）。Mutex 不跨 await（R-e）。
    family_budgets: std::sync::Mutex<family_budget::FamilyBudgetMap>,
    /// 测试钩子：记录每次真正的直连拨号（peer, url, 本轮口径）。
    #[cfg(test)]
    dial_probe: Arc<std::sync::Mutex<Vec<(PeerId, Url, DirectSatisfaction)>>>,
}

impl<H> std::fmt::Debug for DirectConnectorData<H>
where
    H: DirectConnectorHost,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectConnectorData")
            .field("peer_id", &self.peer_manager.my_peer_id())
            .field("network_name", &self.peer_manager.network_name())
            .finish()
    }
}

pub(crate) struct DirectConnectorManager<H>
where
    H: DirectConnectorHost,
{
    data: Arc<DirectConnectorData<H>>,
    client: PeerTaskManager<DirectConnectorLauncher<H>>,
}

impl<H> DirectConnectorManager<H>
where
    H: DirectConnectorHost,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_running_listeners(
        peer_manager: Arc<PeerManagerCore>,
        host: Arc<H>,
        protected_tcp_ports: Arc<ProtectedTcpPortRegistry>,
        stun: Arc<dyn StunSocketMapper<<H as VirtualUdpSocketFactory>::Socket>>,
        running_listeners: Arc<dyn LocalListenerUrls>,
        dns: Arc<dyn DnsResolver>,
        protocol: Arc<
            dyn ClientProtocolUpgrader<<H as crate::socket::tcp::VirtualTcpSocketFactory>::Socket>,
        >,
        options: DirectConnectorOptions,
    ) -> Self {
        let data = Arc::new(DirectConnectorData {
            peer_manager: peer_manager.clone(),
            host,
            protected_tcp_ports,
            stun,
            running_listeners,
            dns,
            protocol,
            options,
            listener_blacklist: ExpiringSet::default(),
            peer_blacklist: ExpiringSet::default(),
            family_budgets: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(test)]
            dial_probe: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let client = PeerTaskManager::new_with_external_signal(
            DirectConnectorLauncher(data.clone()),
            Some(peer_manager.p2p_demand_notify()),
        );
        Self { data, client }
    }

    pub fn run(&self) {
        self.run_as_server();
        self.run_as_client();
    }

    pub fn run_as_server(&self) {
        self.data
            .peer_manager
            .get_peer_rpc_mgr()
            .rpc_server()
            .registry()
            .register(
                GeneratedDirectConnectorRpcServer::new(
                    DirectConnectorRpcHandler::new_with_running_listeners_and_stun(
                        self.data.host.clone(),
                        Some(Arc::downgrade(&self.data.peer_manager)),
                        self.data.running_listeners.clone(),
                        self.data.options.udp_bind.context.clone(),
                        Some(self.data.stun.clone()),
                    ),
                ),
                self.data.peer_manager.network_name(),
            );
    }

    pub fn run_as_client(&self) {
        self.client.start();
    }

    pub async fn stop(&self) {
        self.client.stop().await;
        self.data
            .peer_manager
            .get_peer_rpc_mgr()
            .rpc_server()
            .registry()
            .unregister(
                GeneratedDirectConnectorRpcServer::new(
                    DirectConnectorRpcHandler::new_with_running_listeners_and_stun(
                        self.data.host.clone(),
                        Some(Arc::downgrade(&self.data.peer_manager)),
                        self.data.running_listeners.clone(),
                        self.data.options.udp_bind.context.clone(),
                        Some(self.data.stun.clone()),
                    ),
                ),
                self.data.peer_manager.network_name(),
            );
    }

    pub(crate) async fn local_address_observations_with_stun(
        &self,
        stun_info: &crate::proto::common::StunInfo,
    ) -> GetIpListResponse {
        collect_address_observations(
            self.data.host.as_ref(),
            Some(self.data.peer_manager.as_ref()),
            &self.data.options.udp_bind.context,
            false,
            Some(stun_info),
        )
        .await
    }
}

struct DirectConnectorLauncher<H>(Arc<DirectConnectorData<H>>)
where
    H: DirectConnectorHost;

impl<H> Clone for DirectConnectorLauncher<H>
where
    H: DirectConnectorHost,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[async_trait]
impl<H> PeerTaskLauncher for DirectConnectorLauncher<H>
where
    H: DirectConnectorHost,
{
    type CollectPeerItem = PeerId;
    type TaskRet = ();

    async fn collect_peers_need_task(&self) -> Vec<PeerId> {
        let data = &self.0;
        data.peer_blacklist.cleanup();
        let my_peer_id = data.peer_manager.my_peer_id();
        let policy = data.peer_manager.p2p_policy_flags();
        let now = Instant::now();
        data.peer_manager
            .get_route()
            .list_routes()
            .await
            .into_iter()
            .filter(|route| {
                let static_allowed = should_background_p2p_with_peer(
                    route.feature_flag.as_ref(),
                    data.options.allow_public_server,
                    policy.lazy_p2p,
                    policy.disable_p2p,
                    policy.need_p2p,
                );
                let dynamic_allowed = should_try_p2p_with_peer(
                    route.feature_flag.as_ref(),
                    data.options.allow_public_server,
                    policy.disable_p2p,
                    policy.need_p2p,
                ) && data.peer_manager.has_recent_traffic(route.peer_id, now);
                route.peer_id != my_peer_id
                    && (static_allowed || dynamic_allowed)
                    && !data.peer_satisfied(route.peer_id)
                    && !data.peer_blacklist.contains(&route.peer_id)
                    // R-b：预算过滤 —— 偏好模式下冷却中/已耗尽的 peer 不排任务。
                    && data.family_attempt_allowed(route.peer_id)
            })
            .map(|route| route.peer_id)
            .collect()
    }

    async fn launch_task(&self, peer_id: PeerId) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let data = self.0.clone();
        tokio::spawn(async move { data.try_direct_connect(peer_id).await })
    }

    fn loop_interval_ms(&self) -> u64 {
        DIRECT_TASK_LOOP_INTERVAL_MS
    }
}

impl<H> DirectConnectorData<H>
where
    H: DirectConnectorHost,
{
    /// 直连需求是否已被满足（族感知）。
    ///
    /// - `auto`：与上游完全一致 ——「已有直连连接」即视为满足。
    /// - `prefer_ipv4` / `prefer_ipv6`：只有「已有偏好族连接」才视为满足；
    ///   另一族（例如 IPv6）的连接**不**代表偏好族已打通，应继续尝试偏好族。
    ///
    /// 判定走 `has_conn_of_family`（**含打洞得到的连接**），不用
    /// `has_directly_connected_conn`（它把打洞连接排除在外，会导致
    /// 「打通了还一直重试 + listener 被 300s 拉黑」）。
    fn peer_satisfied(&self, dst_peer_id: PeerId) -> bool {
        self.peer_satisfied_with(dst_peer_id, DirectSatisfaction::Preferred)
    }

    /// 该 peer 当前连接数的同步观测（R-i `conn_snapshot` 的数据源）。
    ///
    /// 直接连接逐条计数；打洞连接按来源（TCP/UDP）存在性各计 1 —— 只用于
    /// 「连接集合是否变化」的比较，不必精确到每一条连接。
    fn family_peer_conn_total(&self, dst_peer_id: PeerId) -> usize {
        let Some(peer) = self.peer_manager.get_peer_map().get_peer_by_id(dst_peer_id) else {
            return 0;
        };
        let mut total = peer.get_directly_connections().len();
        for origin in [
            PeerConnectionOrigin::TcpHolePunch,
            PeerConnectionOrigin::UdpHolePunch,
        ] {
            if peer.has_conn_with_origin(origin) {
                total += 1;
            }
        }
        total
    }

    /// 预算判定（R-b 两个接入点共用）：非偏好模式恒放行；偏好模式下正在
    /// 冷却或已耗尽 → 不放行（冷却在放行判定中自然到期并开新窗口）。
    fn family_attempt_allowed(&self, dst_peer_id: PeerId) -> bool {
        let prefer = self.peer_manager.direct_conn_family();
        if prefer == ConnFamilyPref::Auto {
            return true;
        }
        let conn_total = self.family_peer_conn_total(dst_peer_id);
        let mut budgets = self.family_budgets.lock().unwrap();
        family_budget::family_attempt_allowed(
            &mut budgets,
            dst_peer_id,
            prefer,
            crate::foundation::time::Instant::now(),
            conn_total,
        )
    }

    /// 记一次轮失败（R-d）：一轮打满仍未满足偏好族时调用；
    /// 冷却 / 耗尽在记账点就地 warn（R-g）。
    fn family_round_failure(&self, dst_peer_id: PeerId) {
        let prefer = self.peer_manager.direct_conn_family();
        if prefer == ConnFamilyPref::Auto {
            return;
        }
        let conn_total = self.family_peer_conn_total(dst_peer_id);
        let mut budgets = self.family_budgets.lock().unwrap();
        family_budget::family_round_failure(
            &mut budgets,
            dst_peer_id,
            prefer,
            crate::foundation::time::Instant::now(),
            conn_total,
        );
        if let Some(state) = budgets.get(&dst_peer_id) {
            if state.exhausted {
                tracing::warn!(
                    ?dst_peer_id,
                    exhausted_conn_snapshot = ?state.conn_snapshot,
                    "preferred-family direct connect budget exhausted; paused until \
                     preference or the peer's connection set changes"
                );
            } else if state.cooldown_until.is_some() {
                tracing::warn!(
                    ?dst_peer_id,
                    cooldown = ?family_budget::FAMILY_CONNECT_WINDOW_COOLDOWN,
                    "preferred-family direct connect window exhausted; cooling down"
                );
            }
        }
    }

    /// 记一次成功（R-h 的唯一成功口径：偏好族已满足）。清零该 peer 的预算。
    fn family_attempt_succeeded(&self, dst_peer_id: PeerId) {
        let prefer = self.peer_manager.direct_conn_family();
        if prefer == ConnFamilyPref::Auto {
            return;
        }
        let mut budgets = self.family_budgets.lock().unwrap();
        family_budget::family_attempt_succeeded(&mut budgets, dst_peer_id, prefer);
    }

    /// 直连需求是否已被满足（按本轮的口径判定）。
    ///
    /// - `DirectSatisfaction::Preferred`：与现有 `peer_satisfied` 语义一致 ——
    ///   `auto` 下是「已有直连连接」，`prefer_*` 下是「已有偏好族连接」。
    /// - `DirectSatisfaction::AnyFamily`：任意族已连通即算满足（兜底轮用），
    ///   避免兜底轮为同一 peer 连出多条重复连接。
    fn peer_satisfied_with(&self, dst_peer_id: PeerId, satisfaction: DirectSatisfaction) -> bool {
        match satisfaction {
            DirectSatisfaction::Preferred => {
                let prefer = self.peer_manager.direct_conn_family();
                family_filter::peer_satisfied_by(
                    prefer,
                    |family| self.peer_manager.has_conn_of_family(dst_peer_id, family),
                    || self.peer_manager.has_directly_connected_conn(dst_peer_id),
                )
            }
            DirectSatisfaction::AnyFamily => family_filter::has_any_family_conn(|family| {
                self.peer_manager.has_conn_of_family(dst_peer_id, family)
            }),
        }
    }

    async fn try_direct_connect(self: Arc<Self>, dst_peer_id: PeerId) -> anyhow::Result<()> {
        let backoffs_ms = [1000, 2000, 2000, 5000, 5000, 10000, 30000, 60000];
        let mut backoff_index = 0usize;
        let mut attempt = 0usize;

        loop {
            if self.peer_blacklist.contains(&dst_peer_id) {
                anyhow::bail!("peer {dst_peer_id} is blacklisted");
            }
            if !self.family_attempt_allowed(dst_peer_id) {
                // 预算耗尽或冷却中：安静退出本次任务，等下一次 collect 重新判定
                // （R-b：冷却/耗尽后不再发起直连）。
                tracing::warn!(
                    ?dst_peer_id,
                    "preferred-family direct connect budget unavailable; \
                     abort this round of direct connect attempts"
                );
                return Ok(());
            }
            if attempt > 0 {
                crate::foundation::time::sleep(Duration::from_millis(backoffs_ms[backoff_index]))
                    .await;
                backoff_index = (backoff_index + 1).min(backoffs_ms.len() - 1);
            }
            attempt += 1;

            let rpc_stub = self
                .peer_manager
                .get_peer_rpc_mgr()
                .rpc_client()
                .scoped_client::<DirectConnectorRpcClientFactory<BaseController>>(
                self.peer_manager.my_peer_id(),
                dst_peer_id,
                self.peer_manager.network_name().to_owned(),
            );
            let ip_list = match rpc_stub
                .get_ip_list(BaseController::default(), GetIpListRequest {})
                .await
            {
                Ok(ip_list) => ip_list,
                Err(error @ rpc_types::error::Error::InvalidServiceKey(_, _)) => {
                    self.peer_blacklist
                        .insert(dst_peer_id, INVALID_SERVICE_BLACKLIST_TIMEOUT);
                    return Err(error.into());
                }
                Err(error) => return Err(error.into()),
            };

            tracing::info!(?ip_list, dst_peer_id, "got direct-connect IP list");
            let result = self
                .try_direct_connect_with_ip_list(dst_peer_id, ip_list)
                .await;
            tracing::info!(?result, dst_peer_id, "direct-connect attempt returned");
            if self.peer_satisfied(dst_peer_id) {
                // R-h：只有偏好族已满足才记成功并清零预算（第 4.6 节成功口径）。
                self.family_attempt_succeeded(dst_peer_id);
                return Ok(());
            }
            // R-d：一轮（一次外层退避循环迭代）跑到仍未满足偏好族 → 记一次轮失败。
            self.family_round_failure(dst_peer_id);
        }
    }

    async fn try_direct_connect_with_ip_list(
        self: &Arc<Self>,
        dst_peer_id: PeerId,
        ip_list: GetIpListResponse,
    ) -> anyhow::Result<()> {
        let available_listeners = ip_list
            .listeners
            .clone()
            .into_iter()
            .map(Into::<Url>::into)
            .filter(|listener| listener.scheme() != "ring")
            .filter(|listener| {
                mapped_listener_port(listener).is_some() && listener.host().is_some()
            })
            .filter(|listener| {
                self.options.enable_ipv6 || !matches!(listener.host(), Some(Host::Ipv6(_)))
            })
            .collect::<Vec<_>>();

        // 族偏好分区：偏好族进第一轮，另一族进兜底轮（`auto` 下另一轮恒空）。
        // 偏好族自己的 unspecified listener（`0.0.0.0` / `[::]`）因此被保留 ——
        // 它是本族候选地址的唯一展开源。
        // 域名 listener 留在第一轮：它的族要靠 DNS 解析结果才能确定。
        let prefer = self.peer_manager.direct_conn_family();
        let plan = plan_direct_connect(available_listeners, prefer);

        // 一条 listener 都没有（`auto` 下等价于上游的「无有效 listener」）。
        if plan.preferred.listeners.is_empty() && plan.other.listeners.is_empty() {
            anyhow::bail!("peer {dst_peer_id} has no valid listener");
        }

        // pass 1：偏好族（`auto` 下即全部 listener）。
        if self
            .run_direct_connect_pass(dst_peer_id, &ip_list, plan.preferred)
            .await
        {
            return Ok(());
        }

        // 兜底门控：偏好族没打通、且该 peer 当前一条连接都没有时，
        // 允许再用另一族 listener 建一条连接（避免把只提供另一族 listener 的 peer
        // 变成完全不可直连）。
        let has_any_family_conn =
            self.peer_satisfied_with(dst_peer_id, DirectSatisfaction::AnyFamily);
        if fallback_to_other_family_allowed(prefer, plan.other.listeners.len(), has_any_family_conn)
        {
            tracing::info!(
                ?dst_peer_id,
                "fallback to other address family for direct connect"
            );
            if self
                .run_direct_connect_pass(dst_peer_id, &ip_list, plan.other)
                .await
            {
                return Ok(());
            }
        }

        Ok(())
    }

    /// 跑一轮直连：按 scheme 排序 → 逐组并发 → 每组结束判满足。
    ///
    /// 返回该轮是否已满足（连上了）。
    async fn run_direct_connect_pass(
        self: &Arc<Self>,
        dst_peer_id: PeerId,
        ip_list: &GetIpListResponse,
        pass: DirectConnectPass,
    ) -> bool {
        let mut available_listeners = pass.listeners;

        available_listeners.sort_by_key(|listener| {
            if listener.scheme() == self.options.default_protocol {
                3
            } else if listener.scheme() == "udp" {
                2
            } else {
                1
            }
        });

        while !available_listeners.is_empty() {
            let mut tasks = JoinSet::new();
            let current_scheme = available_listeners
                .last()
                .expect("non-empty listener list")
                .scheme()
                .to_owned();
            while available_listeners
                .last()
                .is_some_and(|listener| listener.scheme() == current_scheme)
            {
                let listener = available_listeners.pop().expect("listener should exist");
                self.spawn_direct_connect_tasks(
                    dst_peer_id,
                    ip_list,
                    &listener,
                    &mut tasks,
                    pass.satisfaction,
                    pass.ip_version,
                )
                .await;
            }
            let _ = tasks.join_all().await;
            if self.peer_satisfied_with(dst_peer_id, pass.satisfaction) {
                return true;
            }
        }
        false
    }

    async fn spawn_direct_connect_tasks(
        self: &Arc<Self>,
        dst_peer_id: PeerId,
        ip_list: &GetIpListResponse,
        listener: &Url,
        tasks: &mut JoinSet<anyhow::Result<()>>,
        satisfaction: DirectSatisfaction,
        ip_version: IpVersion,
    ) {
        let Ok(mut addrs) = resolve_mapped_listener_addrs(
            listener,
            self.options.tcp_bind.context.clone(),
            self.dns.as_ref(),
        )
        .await
        else {
            tracing::error!(?listener, "failed to resolve direct listener");
            return;
        };
        let listener_host = addrs.pop();
        let is_udp = protocol_uses_udp(listener.scheme());
        let local_listeners = self.running_listeners.local_listener_urls();
        let port_has_local_listener = |port: u16| {
            local_listeners.iter().any(|local| {
                local.port() == Some(port) && protocol_uses_udp(local.scheme()) == is_udp
            })
        };
        let should_deny_target = |target: &SocketAddr| {
            let port_is_protected = port_has_local_listener(target.port())
                || (!is_udp && self.protected_tcp_ports.contains(target.port()));
            port_is_protected && self.host.is_local_ip(&target.ip())
        };

        match listener_host {
            Some(SocketAddr::V4(socket_addr)) if socket_addr.ip().is_unspecified() => {
                for ip in ip_list
                    .interface_ipv4s
                    .iter()
                    .chain(ip_list.public_ipv4.iter())
                {
                    let target = SocketAddr::new(IpAddr::V4(ip.addr.into()), socket_addr.port());
                    if should_deny_target(&target) {
                        continue;
                    }
                    let mut url = listener.clone();
                    if url.set_ip_host(target.ip()).is_ok() {
                        tasks.spawn(Self::try_connect_to_url(
                            self.clone(),
                            dst_peer_id,
                            url.to_string(),
                            satisfaction,
                            ip_version,
                        ));
                    }
                }
            }
            Some(SocketAddr::V4(socket_addr))
                if (!socket_addr.ip().is_loopback() || self.options.testing)
                    && !should_deny_target(&SocketAddr::V4(socket_addr)) =>
            {
                tasks.spawn(Self::try_connect_to_url(
                    self.clone(),
                    dst_peer_id,
                    listener.to_string(),
                    satisfaction,
                    ip_version,
                ));
            }
            Some(SocketAddr::V6(socket_addr)) if socket_addr.ip().is_unspecified() => {
                let mut candidates = HashSet::new();
                for ip in ip_list
                    .interface_ipv6s
                    .iter()
                    .chain(ip_list.public_ipv6.iter())
                    .map(|ip| Ipv6Addr::from(*ip))
                {
                    if self.is_usable_public_ipv6(&ip).await {
                        candidates.insert(ip);
                    }
                }
                for ip in candidates {
                    let target = SocketAddr::new(IpAddr::V6(ip), socket_addr.port());
                    if should_deny_target(&target) {
                        continue;
                    }
                    let mut url = listener.clone();
                    if url.set_ip_host(target.ip()).is_ok() {
                        tasks.spawn(Self::try_connect_to_url(
                            self.clone(),
                            dst_peer_id,
                            url.to_string(),
                            satisfaction,
                            ip_version,
                        ));
                    }
                }
            }
            Some(SocketAddr::V6(socket_addr))
                if self
                    .peer_manager
                    .is_easytier_managed_ipv6(socket_addr.ip())
                    .await =>
            {
                tracing::debug!(?listener, "skip managed IPv6 direct target");
            }
            Some(SocketAddr::V6(socket_addr))
                if (!socket_addr.ip().is_loopback() || self.options.testing)
                    && !should_deny_target(&SocketAddr::V6(socket_addr)) =>
            {
                tasks.spawn(Self::try_connect_to_url(
                    self.clone(),
                    dst_peer_id,
                    listener.to_string(),
                    satisfaction,
                    ip_version,
                ));
            }
            _ => {}
        }
    }

    async fn try_connect_to_url(
        self: Arc<Self>,
        dst_peer_id: PeerId,
        url: String,
        satisfaction: DirectSatisfaction,
        ip_version: IpVersion,
    ) -> anyhow::Result<()> {
        self.listener_blacklist.cleanup();
        let key = ListenerBlacklistKey(dst_peer_id, url.clone());
        if self.listener_blacklist.contains(&key) {
            anyhow::bail!("direct listener URL is blacklisted");
        }

        let backoffs_ms = [1000i64, 2000, 4000];
        for attempt in 0..=backoffs_ms.len() {
            if self.peer_satisfied_with(dst_peer_id, satisfaction) {
                return Ok(());
            }
            let result = self
                .connect_to_url_once(dst_peer_id, &url, satisfaction, ip_version)
                .await;
            if result.is_ok() || self.peer_satisfied_with(dst_peer_id, satisfaction) {
                return Ok(());
            }
            if attempt == backoffs_ms.len() {
                self.listener_blacklist
                    .insert(key, DIRECT_CONNECTOR_BLACKLIST_TIMEOUT);
                return result;
            }

            let base = backoffs_ms[attempt];
            let delta = base >> 1;
            let delay_ms = {
                let mut rng = rand::thread_rng();
                base + rng.gen_range(-delta..delta)
            };
            crate::foundation::time::sleep(Duration::from_millis(delay_ms as u64)).await;
        }
        unreachable!("direct URL retry loop must return")
    }

    async fn connect_to_url_once(
        &self,
        dst_peer_id: PeerId,
        raw_url: &str,
        satisfaction: DirectSatisfaction,
        ip_version: IpVersion,
    ) -> anyhow::Result<()> {
        if self.peer_satisfied_with(dst_peer_id, satisfaction) {
            // 本轮口径下已经连上了（或上一轮连上了），不再重复建连。
            return Ok(());
        }
        let url = Url::parse(raw_url)?;
        #[cfg(test)]
        self.dial_probe
            .lock()
            .unwrap()
            .push((dst_peer_id, url.clone(), satisfaction));
        let (peer_id, conn_id) = if url.scheme() == "udp" {
            match url.host() {
                Some(Host::Ipv6(_)) => self.connect_public_ipv6(dst_peer_id, &url).await?,
                Some(Host::Ipv4(ip)) if is_public_ipv4(ip) => {
                    match self.connect_public_ipv4(dst_peer_id, &url).await {
                        Ok(result) => result,
                        Err(error) => {
                            tracing::debug!(?error, %url, "public IPv4 UDP punch failed; fallback");
                            self.connect_ordinary(dst_peer_id, url.clone(), ip_version)
                                .await?
                        }
                    }
                }
                _ => {
                    self.connect_ordinary(dst_peer_id, url.clone(), ip_version)
                        .await?
                }
            }
        } else {
            self.connect_ordinary(dst_peer_id, url.clone(), ip_version)
                .await?
        };

        if peer_id != dst_peer_id && !self.options.testing {
            self.peer_manager.close_peer_conn(peer_id, &conn_id).await?;
            anyhow::bail!("direct peer mismatch for {url}: expected {dst_peer_id}, got {peer_id}");
        }
        Ok(())
    }

    async fn connect_ordinary(
        &self,
        dst_peer_id: PeerId,
        url: Url,
        ip_version: IpVersion,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        let transport = DirectTransport::from_url(&url)?;
        let normalized = convert_idn_to_ascii(url.clone())?;
        let default_port = mapped_listener_port(&normalized)
            .ok_or_else(|| anyhow::anyhow!("listener has no port: {url}"))?;
        let remote_addr = resolve_remote_addr(
            self.peer_manager.as_ref(),
            self.host.as_ref(),
            self.dns.as_ref(),
            &normalized,
            default_port,
            self.options.socket_context(transport, ip_version),
        )
        .await?;
        let bind_addrs = if self.options.bind_device
            && self.options.allow_interface_bind
            && transport.supports_interface_bind()
        {
            collect_bind_addrs(
                self.peer_manager.as_ref(),
                self.host.as_ref(),
                transport.is_udp(),
                remote_addr,
            )
            .await?
        } else {
            Vec::new()
        };
        crate::foundation::time::timeout(DIRECT_CONNECT_TIMEOUT, async {
            let connected = match transport {
                DirectTransport::Tcp(purpose) => ConnectedTransport::Tcp(
                    transport::connect_tcp(
                        self.host.clone(),
                        remote_addr,
                        bind_addrs,
                        self.options.tcp_bind.clone(),
                        purpose,
                    )
                    .await?,
                ),
                DirectTransport::Udp(mode) => ConnectedTransport::Udp(
                    transport::connect_udp(
                        self.host.clone(),
                        remote_addr,
                        bind_addrs,
                        self.options.udp_bind.clone(),
                        mode,
                    )
                    .await?,
                ),
            };
            let tunnel = self.protocol.upgrade_client(connected, url).await?;
            self.admit(tunnel, dst_peer_id).await
        })
        .await?
    }

    async fn connect_public_ipv4(
        &self,
        dst_peer_id: PeerId,
        url: &Url,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        let socket = self
            .host
            .bind_udp(
                UdpBindOptions::direct_connect()
                    .with_context(
                        self.options
                            .udp_bind
                            .context
                            .clone()
                            .with_ip_version(IpVersion::V4),
                    )
                    .with_local_addr(Some("0.0.0.0:0".parse().unwrap())),
            )
            .await?;
        let connector_addr = self
            .stun
            .get_udp_port_mapping_with_socket(socket.clone())
            .await?;
        let _ = self
            .remote_send_udp_hole_punch_packet(dst_peer_id, vec![connector_addr], None, url)
            .await;
        let remote_addr = resolve_literal_url(url, IpVersion::V4)?;
        let connected = udp::connect_with_socket(self.host.clone(), socket, remote_addr).await?;
        let tunnel = self
            .protocol
            .upgrade_client(ConnectedTransport::Udp(connected), url.clone())
            .await?;
        self.admit(tunnel, dst_peer_id).await
    }

    async fn connect_public_ipv6(
        &self,
        dst_peer_id: PeerId,
        url: &Url,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        let socket = self
            .host
            .bind_udp(
                UdpBindOptions::direct_connect()
                    .with_context(
                        self.options
                            .udp_bind
                            .context
                            .clone()
                            .with_ip_version(IpVersion::V6),
                    )
                    .with_local_addr(Some("[::]:0".parse().unwrap())),
            )
            .await?;
        let connector_ips = self.collect_ipv6_hole_punch_candidates().await?;
        if !connector_ips.is_empty() {
            let port = socket.local_addr()?.port();
            let connector_addrs = connector_ips
                .into_iter()
                .map(|ip| SocketAddr::new(IpAddr::V6(ip), port))
                .collect();
            let preferred_src = match url.host() {
                Some(Host::Ipv6(ip)) => Some(ip),
                _ => None,
            };
            let _ = self
                .remote_send_udp_hole_punch_packet(dst_peer_id, connector_addrs, preferred_src, url)
                .await;
        }
        let remote_addr = resolve_literal_url(url, IpVersion::V6)?;
        let connected = udp::connect_with_socket(self.host.clone(), socket, remote_addr).await?;
        let tunnel = self
            .protocol
            .upgrade_client(ConnectedTransport::Udp(connected), url.clone())
            .await?;
        self.admit(tunnel, dst_peer_id).await
    }

    async fn collect_ipv6_hole_punch_candidates(&self) -> anyhow::Result<Vec<Ipv6Addr>> {
        let mut candidates = Vec::new();
        for ip in self
            .stun
            .get_stun_info()
            .public_ip
            .into_iter()
            .filter_map(|ip| ip.parse().ok())
        {
            if let IpAddr::V6(ip) = ip {
                self.push_ipv6_candidate(&mut candidates, ip).await;
            }
        }
        let interface_addrs = self.host.interface_addrs().await?;
        for ip in interface_addrs
            .interface_ipv6s
            .into_iter()
            .chain(interface_addrs.public_ipv6)
        {
            self.push_ipv6_candidate(&mut candidates, ip).await;
        }
        Ok(candidates)
    }

    async fn push_ipv6_candidate(&self, candidates: &mut Vec<Ipv6Addr>, ip: Ipv6Addr) {
        if candidates.len() < MAX_IPV6_HOLE_PUNCH_CONNECTOR_ADDRS
            && self.is_usable_public_ipv6(&ip).await
            && !candidates.contains(&ip)
        {
            candidates.push(ip);
        }
    }

    async fn is_usable_public_ipv6(&self, ip: &Ipv6Addr) -> bool {
        !self.peer_manager.is_easytier_managed_ipv6(ip).await
            && (self.options.testing
                || (!ip.is_loopback()
                    && !ip.is_unspecified()
                    && !ip.is_unique_local()
                    && !ip.is_unicast_link_local()
                    && !ip.is_multicast()))
    }

    async fn remote_send_udp_hole_punch_packet(
        &self,
        dst_peer_id: PeerId,
        connector_addrs: Vec<SocketAddr>,
        preferred_src_ipv6: Option<Ipv6Addr>,
        remote_url: &Url,
    ) -> anyhow::Result<()> {
        if remote_url.scheme() != "udp" {
            anyhow::bail!("UDP punch request requires a UDP listener: {remote_url}");
        }
        let listener_port = mapped_listener_port(remote_url)
            .ok_or_else(|| anyhow::anyhow!("listener has no port: {remote_url}"))?;
        let rpc_stub = self
            .peer_manager
            .get_peer_rpc_mgr()
            .rpc_client()
            .scoped_client::<DirectConnectorRpcClientFactory<BaseController>>(
            self.peer_manager.my_peer_id(),
            dst_peer_id,
            self.peer_manager.network_name().to_owned(),
        );
        rpc_stub
            .send_udp_hole_punch_packet(
                BaseController::default(),
                SendUdpHolePunchPacketRequest {
                    connector_addr: connector_addrs.first().copied().map(Into::into),
                    listener_port: listener_port as u32,
                    preferred_src_ipv6: preferred_src_ipv6.map(Into::into),
                    connector_addrs: connector_addrs.into_iter().map(Into::into).collect(),
                },
            )
            .await
            .with_context(|| format!("send UDP punch request to peer {dst_peer_id}"))?;
        Ok(())
    }

    async fn admit(
        &self,
        tunnel: Box<dyn Tunnel>,
        dst_peer_id: PeerId,
    ) -> anyhow::Result<(PeerId, PeerConnId)> {
        self.peer_manager
            .add_client_tunnel_with_peer_id_hint(
                tunnel,
                PeerConnectionOrigin::Direct,
                Some(dst_peer_id),
            )
            .await
            .map_err(Into::into)
    }
}

pub(crate) struct DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    host: Arc<H>,
    peer_manager: Option<Weak<PeerManagerCore>>,
    running_listeners: Arc<dyn LocalListenerUrls>,
    socket_context: SocketContext,
    foreign_network: bool,
    stun: Option<Arc<dyn StunInfoProvider>>,
}

impl<H> Clone for DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            peer_manager: self.peer_manager.clone(),
            running_listeners: self.running_listeners.clone(),
            socket_context: self.socket_context.clone(),
            foreign_network: self.foreign_network,
            stun: self.stun.clone(),
        }
    }
}

impl<H> DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    pub fn new_for_foreign_network_with_stun(
        host: Arc<H>,
        running_listeners: Arc<dyn LocalListenerUrls>,
        socket_context: SocketContext,
        stun: Option<Arc<dyn StunInfoProvider>>,
    ) -> Self {
        Self {
            host,
            peer_manager: None,
            running_listeners,
            socket_context,
            foreign_network: true,
            stun,
        }
    }

    fn new_with_running_listeners_and_stun(
        host: Arc<H>,
        peer_manager: Option<Weak<PeerManagerCore>>,
        running_listeners: Arc<dyn LocalListenerUrls>,
        socket_context: SocketContext,
        stun: Option<Arc<dyn StunInfoProvider>>,
    ) -> Self {
        Self {
            host,
            peer_manager,
            running_listeners,
            socket_context,
            foreign_network: false,
            stun,
        }
    }
}

async fn collect_address_observations<H>(
    host: &H,
    peer_manager: Option<&PeerManagerCore>,
    socket_context: &SocketContext,
    foreign_network: bool,
    stun_info: Option<&crate::proto::common::StunInfo>,
) -> GetIpListResponse
where
    H: DirectConnectorHost,
{
    let mut response = if foreign_network {
        host.collect_foreign_ip_addrs(socket_context).await
    } else {
        host.collect_ip_addrs(socket_context).await
    };
    if let Some(stun_info) = stun_info {
        for public_ip in &stun_info.public_ip {
            match public_ip.parse::<IpAddr>() {
                Ok(IpAddr::V4(ip)) => response.public_ipv4 = Some(ip.into()),
                Ok(IpAddr::V6(ip)) => response.public_ipv6 = Some(ip.into()),
                Err(_) => {}
            }
        }
    }
    if let Some(peer_manager) = peer_manager.filter(|_| !foreign_network) {
        let mut interface_ipv6s = Vec::with_capacity(response.interface_ipv6s.len());
        for ip in response.interface_ipv6s {
            if !peer_manager
                .is_easytier_managed_ipv6(&Ipv6Addr::from(ip))
                .await
            {
                interface_ipv6s.push(ip);
            }
        }
        response.interface_ipv6s = interface_ipv6s;
        if let Some(ip) = response.public_ipv6.map(Ipv6Addr::from)
            && peer_manager.is_easytier_managed_ipv6(&ip).await
        {
            response.public_ipv6 = None;
        }
    }
    response
}

pub(crate) struct ForeignDirectConnectorRpcRegistrar<H>
where
    H: DirectConnectorHost,
{
    host: Arc<H>,
    stun: Arc<dyn StunInfoProvider>,
    running_listeners: Arc<dyn LocalListenerUrls>,
}

impl<H> ForeignDirectConnectorRpcRegistrar<H>
where
    H: DirectConnectorHost,
{
    pub fn new(
        host: Arc<H>,
        stun: Arc<dyn StunInfoProvider>,
        running_listeners: Arc<dyn LocalListenerUrls>,
    ) -> Self {
        Self {
            host,
            stun,
            running_listeners,
        }
    }
}

impl<H> ForeignNetworkRpcRegistrar for ForeignDirectConnectorRpcRegistrar<H>
where
    H: DirectConnectorHost,
{
    fn register_peer_rpc_services(
        &self,
        peer_rpc: &Arc<PeerRpcManager>,
        network_name: &str,
        socket_context: SocketContext,
    ) {
        peer_rpc.rpc_server().registry().register(
            GeneratedDirectConnectorRpcServer::new(
                DirectConnectorRpcHandler::new_for_foreign_network_with_stun(
                    self.host.clone(),
                    self.running_listeners.clone(),
                    socket_context,
                    Some(self.stun.clone()),
                ),
            ),
            network_name,
        );
    }
}

#[async_trait]
impl<H> DirectConnectorRpc for DirectConnectorRpcHandler<H>
where
    H: DirectConnectorHost,
{
    type Controller = BaseController;

    async fn get_ip_list(
        &self,
        _: BaseController,
        _: GetIpListRequest,
    ) -> rpc_types::error::Result<GetIpListResponse> {
        let peer_manager = self.peer_manager.as_ref().and_then(Weak::upgrade);
        let mut response = collect_address_observations(
            self.host.as_ref(),
            peer_manager.as_deref(),
            &self.socket_context,
            self.foreign_network,
            self.stun
                .as_deref()
                .map(StunInfoProvider::get_stun_info)
                .as_ref(),
        )
        .await;
        response.listeners = self
            .host
            .mapped_listeners()
            .into_iter()
            .chain(self.running_listeners.local_listener_urls())
            .map(Into::into)
            .collect();
        Ok(response)
    }

    async fn send_udp_hole_punch_packet(
        &self,
        _: BaseController,
        request: SendUdpHolePunchPacketRequest,
    ) -> rpc_types::error::Result<Void> {
        let (listener_port, connector_addrs, preferred_src_ipv6) =
            connector_addrs_from_request(request)?;
        let peer_manager = self.peer_manager.as_ref().and_then(Weak::upgrade);
        let preferred_source = match preferred_src_ipv6.map(Ipv6Addr::from) {
            Some(ip) => {
                if self.foreign_network {
                    self.host
                        .preferred_foreign_ipv6_source(ip, self.socket_context.clone())
                        .await
                } else if let Some(peer_manager) = peer_manager.as_deref()
                    && peer_manager.is_easytier_managed_ipv6(&ip).await
                {
                    None
                } else {
                    self.host
                        .preferred_ipv6_source(ip, self.socket_context.clone())
                        .await
                }
            }
            None => None,
        };

        for _ in 0..3 {
            for connector_addr in &connector_addrs {
                let result = match connector_addr {
                    SocketAddr::V4(addr) => {
                        send_v4_hole_punch_control_packet(
                            self.host.as_ref(),
                            self.socket_context.clone(),
                            listener_port,
                            *addr,
                        )
                        .await
                    }
                    SocketAddr::V6(addr) => {
                        send_v6_hole_punch_control_packet(
                            self.host.as_ref(),
                            self.socket_context.clone(),
                            listener_port,
                            *addr,
                            preferred_source,
                        )
                        .await
                    }
                };
                if let Err(error) = result {
                    tracing::debug!(?error, ?connector_addr, "send UDP punch packet failed");
                }
            }
            crate::foundation::time::sleep(Duration::from_millis(30)).await;
        }
        Ok(Void::default())
    }
}

fn connector_addrs_from_request(
    request: SendUdpHolePunchPacketRequest,
) -> rpc_types::error::Result<(u16, Vec<SocketAddr>, Option<crate::proto::common::Ipv6Addr>)> {
    let listener_port = u16::try_from(request.listener_port)
        .map_err(|_| anyhow::anyhow!("listener_port out of range: {}", request.listener_port))?;
    let mut connector_addrs = request
        .connector_addrs
        .into_iter()
        .map(SocketAddr::from)
        .collect::<Vec<_>>();
    if connector_addrs.is_empty() {
        connector_addrs.push(
            request
                .connector_addr
                .ok_or_else(|| anyhow::anyhow!("connector_addr is required"))?
                .into(),
        );
    }
    let mut deduped = Vec::with_capacity(connector_addrs.len());
    for addr in connector_addrs {
        if !deduped.contains(&addr) {
            deduped.push(addr);
        }
        if deduped.len() >= MAX_UDP_HOLE_PUNCH_CONNECTOR_ADDRS {
            break;
        }
    }
    Ok((listener_port, deduped, request.preferred_src_ipv6))
}

fn mapped_listener_port(url: &Url) -> Option<u16> {
    url.port()
        .or_else(|| crate::connectivity::protocol::protocol_default_port(url.scheme()))
}

async fn resolve_mapped_listener_addrs(
    listener: &Url,
    context: SocketContext,
    dns: &dyn DnsResolver,
) -> anyhow::Result<Vec<SocketAddr>> {
    let port = mapped_listener_port(listener)
        .ok_or_else(|| anyhow::anyhow!("listener has no default port: {listener}"))?;
    resolve_url_addrs(
        listener,
        port,
        context.with_ip_version(IpVersion::Both),
        dns,
    )
    .await
}

fn resolve_literal_url(url: &Url, ip_version: IpVersion) -> anyhow::Result<SocketAddr> {
    let port =
        mapped_listener_port(url).ok_or_else(|| anyhow::anyhow!("listener has no port: {url}"))?;
    match (url.host(), ip_version) {
        (Some(Host::Ipv4(ip)), IpVersion::V4 | IpVersion::Both) => {
            Ok(SocketAddr::new(IpAddr::V4(ip), port))
        }
        (Some(Host::Ipv6(ip)), IpVersion::V6 | IpVersion::Both) => {
            Ok(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => anyhow::bail!("URL host does not match {ip_version:?}: {url}"),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_private()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_broadcast()
        && !ip.is_unspecified()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectivity::NoLocalListeners;

    impl<H> DirectConnectorRpcHandler<H>
    where
        H: DirectConnectorHost,
    {
        pub(crate) fn new_with_stun(
            host: Arc<H>,
            socket_context: SocketContext,
            stun: Option<Arc<dyn StunInfoProvider>>,
        ) -> Self {
            let running_listeners = Arc::new(NoLocalListeners);
            Self {
                host,
                peer_manager: None,
                running_listeners,
                socket_context,
                foreign_network: false,
                stun,
            }
        }
    }

    #[test]
    fn faketcp_uses_specialized_tcp_socket_without_interface_binding() {
        let transport =
            DirectTransport::from_url(&"faketcp://127.0.0.1:11013".parse().unwrap()).unwrap();

        assert_eq!(transport, DirectTransport::Tcp(TcpSocketPurpose::FakeTcp));
        assert!(!transport.supports_interface_bind());
        assert!(!transport.is_udp());
    }

    #[test]
    fn connector_address_request_deduplicates_and_caps() {
        let mut request = SendUdpHolePunchPacketRequest {
            listener_port: 11010,
            ..Default::default()
        };
        for port in 1..=20 {
            request
                .connector_addrs
                .push(SocketAddr::from(([127, 0, 0, 1], port)).into());
        }
        request
            .connector_addrs
            .push(SocketAddr::from(([127, 0, 0, 1], 1)).into());

        let (_, addrs, _) = connector_addrs_from_request(request).unwrap();
        assert_eq!(addrs.len(), MAX_UDP_HOLE_PUNCH_CONNECTOR_ADDRS);
        assert_eq!(addrs[0], SocketAddr::from(([127, 0, 0, 1], 1)));
    }

    #[test]
    fn expiring_set_removes_expired_entries() {
        let set = ExpiringSet::default();
        set.insert(7u32, Duration::ZERO);
        assert!(!set.contains(&7));
    }

    fn ipv6_listener_url() -> Url {
        Url::parse("tcp://[2001:db8::1]:11010").unwrap()
    }

    #[test]
    fn falls_back_to_other_family_when_preferred_family_has_no_listener() {
        // 偏好 IPv4，但对端只提供 IPv6 listener。
        let listeners = vec![ipv6_listener_url()];
        let plan = plan_direct_connect(listeners, ConnFamilyPref::PreferIpv4);

        // 第一轮（偏好族）没有可试的 listener。
        assert!(plan.preferred.listeners.is_empty());
        assert_eq!(plan.preferred.satisfaction, DirectSatisfaction::Preferred);
        assert_eq!(plan.preferred.ip_version, IpVersion::V4);

        // 兜底轮用另一族的 listener：任意族连通即算满足，socket 解析放行两族。
        assert_eq!(plan.other.listeners.len(), 1);
        assert_eq!(
            plan.other.listeners[0].as_str(),
            "tcp://[2001:db8::1]:11010"
        );
        assert_eq!(plan.other.satisfaction, DirectSatisfaction::AnyFamily);
        assert_eq!(plan.other.ip_version, IpVersion::Both);

        // 门控：偏好非 auto、另一族非空、该 peer 还没有任何族的连接。
        assert!(fallback_to_other_family_allowed(
            ConnFamilyPref::PreferIpv4,
            plan.other.listeners.len(),
            false
        ));
    }

    #[test]
    fn does_not_fall_back_when_peer_already_has_a_connection() {
        let listeners = vec![ipv6_listener_url()];
        let plan = plan_direct_connect(listeners, ConnFamilyPref::PreferIpv4);
        // 该 peer 已经有一条连接（任何族）→ 不再新建重复连接。
        assert!(!fallback_to_other_family_allowed(
            ConnFamilyPref::PreferIpv4,
            plan.other.listeners.len(),
            true
        ));
    }

    #[test]
    fn does_not_fall_back_when_other_family_has_no_listener() {
        // 偏好 IPv4，但只提供 IPv4 listener → 没有兜底目标。
        let listeners = vec![Url::parse("tcp://1.2.3.4:11010").unwrap()];
        let plan = plan_direct_connect(listeners, ConnFamilyPref::PreferIpv4);
        assert_eq!(plan.preferred.listeners.len(), 1);
        assert!(plan.other.listeners.is_empty());
        assert!(!fallback_to_other_family_allowed(
            ConnFamilyPref::PreferIpv4,
            plan.other.listeners.len(),
            false
        ));
    }

    #[test]
    fn auto_never_falls_back_to_other_family() {
        let listeners = vec![
            Url::parse("udp://0.0.0.0:11010").unwrap(),
            Url::parse("udp://[::]:11010").unwrap(),
            Url::parse("tcp://example.com:11010").unwrap(),
        ];
        let plan = plan_direct_connect(listeners, ConnFamilyPref::Auto);

        // Auto：全部 listener 留在第一轮，另一轮恒为空 → 永远不会兜底。
        assert_eq!(plan.preferred.listeners.len(), 3);
        assert!(plan.other.listeners.is_empty());
        assert!(!fallback_to_other_family_allowed(
            ConnFamilyPref::Auto,
            plan.other.listeners.len(),
            false
        ));

        // Auto 的第一轮与上游逐位一致：解析放行两族、「已有直连连接」即算满足。
        assert_eq!(plan.preferred.satisfaction, DirectSatisfaction::Preferred);
        assert_eq!(plan.preferred.ip_version, IpVersion::Both);
    }

    #[test]
    fn fallback_pass_excludes_domain_listeners() {
        // 域名的族要靠 DNS 解析结果决定，无法按轮次区分 → 留在第一轮，不进兜底轮。
        let listeners = vec![
            Url::parse("tcp://example.com:11010").unwrap(),
            ipv6_listener_url(),
        ];
        let plan = plan_direct_connect(listeners, ConnFamilyPref::PreferIpv4);
        assert_eq!(plan.preferred.listeners.len(), 1);
        assert_eq!(
            plan.preferred.listeners[0].as_str(),
            "tcp://example.com:11010"
        );
        assert_eq!(plan.other.listeners.len(), 1);
        assert_eq!(
            plan.other.listeners[0].as_str(),
            "tcp://[2001:db8::1]:11010"
        );
    }

    // ---- T4 正向路径集成用例：预算与兜底轮在同一套拨号链路上共存 ----

    use crate::config::peers::{HostRoutingPolicy, PeerRuntimeConfig};
    use crate::config::{CoreConfig, NetworkIdentity};
    use crate::connectivity::hole_punch::udp::{
        UdpHolePunchRuntime, UdpPunchListener, UdpPunchSocket, UdpResolvedPublicAddr,
    };
    use crate::host::testkit::{TestDns, TestHost, TestUdpSocket};
    use crate::peers::peer_manager::PortablePeerManagerConfig;
    use crate::proto::common::{StunInfo, Url as ProtoUrl};
    use crate::socket::tcp::{TcpConnectOptions, VirtualTcpSocketFactory};
    use crate::socket::udp::UdpBindOptions;

    struct TestStun;

    #[async_trait::async_trait]
    impl StunInfoProvider for TestStun {
        fn get_stun_info(&self) -> StunInfo {
            StunInfo::default()
        }

        async fn get_udp_port_mapping(&self, _local_port: u16) -> anyhow::Result<SocketAddr> {
            anyhow::bail!("budget test does not map udp ports")
        }

        async fn get_tcp_port_mapping(&self, _local_port: u16) -> anyhow::Result<SocketAddr> {
            anyhow::bail!("budget test does not map tcp ports")
        }

        fn update_stun_info(&self) {}
    }

    #[async_trait::async_trait]
    impl StunSocketMapper<TestUdpSocket> for TestStun {
        async fn get_udp_port_mapping_with_socket(
            &self,
            _socket: Arc<TestUdpSocket>,
        ) -> anyhow::Result<SocketAddr> {
            anyhow::bail!("budget test does not map udp ports")
        }
    }

    struct TestUpgrader;

    #[async_trait::async_trait]
    impl
        crate::connectivity::protocol::ClientProtocolUpgrader<
            <TestHost as crate::socket::tcp::VirtualTcpSocketFactory>::Socket,
        > for TestUpgrader
    {
        fn supports_scheme(&self, _scheme: &str) -> bool {
            false
        }

        async fn upgrade_client(
            &self,
            _connected: crate::connectivity::transport::ConnectedTransport<
                <TestHost as crate::socket::tcp::VirtualTcpSocketFactory>::Socket,
            >,
            _requested_url: Url,
        ) -> anyhow::Result<Box<dyn crate::tunnel::Tunnel>> {
            anyhow::bail!("budget test never upgrades (all dials fail)")
        }
    }

    struct TestDirectHost(Arc<TestHost>);

    #[async_trait::async_trait]
    impl VirtualTcpSocketFactory for TestDirectHost {
        type Socket = <TestHost as VirtualTcpSocketFactory>::Socket;

        async fn connect_tcp(&self, options: TcpConnectOptions) -> anyhow::Result<Self::Socket> {
            self.0.connect_tcp(options).await
        }
    }

    #[async_trait::async_trait]
    impl UdpHolePunchRuntime for TestDirectHost {
        type Socket = TestUdpSocket;

        async fn bind_udp(&self, _options: UdpBindOptions) -> anyhow::Result<Arc<TestUdpSocket>> {
            anyhow::bail!("budget test does not punch udp")
        }

        async fn resolve_udp_public_addr(
            &self,
            _socket: Arc<TestUdpSocket>,
        ) -> anyhow::Result<UdpResolvedPublicAddr> {
            anyhow::bail!("budget test does not punch udp")
        }

        async fn create_listener(
            &self,
            _prefer_port_mapping: bool,
        ) -> anyhow::Result<UdpPunchListener<TestUdpSocket>> {
            anyhow::bail!("budget test does not punch udp")
        }

        async fn create_port_bound_listener(
            &self,
            _port: u16,
        ) -> anyhow::Result<UdpPunchListener<TestUdpSocket>> {
            anyhow::bail!("budget test does not punch udp")
        }

        async fn connect_with_socket(
            &self,
            _socket: Arc<TestUdpSocket>,
            _remote: SocketAddr,
        ) -> anyhow::Result<UdpPunchSocket> {
            anyhow::bail!("budget test does not punch udp")
        }
    }

    #[async_trait::async_trait]
    impl ManualConnectorHost for TestDirectHost {
        async fn local_addr_for_remote(
            &self,
            remote_addr: SocketAddr,
            _context: SocketContext,
        ) -> anyhow::Result<SocketAddr> {
            Ok(match remote_addr {
                SocketAddr::V4(_) => "127.0.0.1:0".parse().unwrap(),
                SocketAddr::V6(_) => "[::1]:0".parse().unwrap(),
            })
        }

        async fn interface_addrs(
            &self,
        ) -> anyhow::Result<crate::connectivity::manual::ManualInterfaceAddrs> {
            Ok(crate::connectivity::manual::ManualInterfaceAddrs {
                interface_ipv4s: vec![],
                interface_ipv6s: vec![],
                public_ipv6: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl DirectConnectorHost for TestDirectHost {
        async fn collect_ip_addrs(&self, _context: &SocketContext) -> GetIpListResponse {
            GetIpListResponse::default()
        }

        fn mapped_listeners(&self) -> Vec<Url> {
            Vec::new()
        }

        fn is_local_ip(&self, _ip: &IpAddr) -> bool {
            false
        }

        async fn preferred_ipv6_source(
            &self,
            _ip: Ipv6Addr,
            _context: SocketContext,
        ) -> Option<PreferredIpv6Source> {
            None
        }
    }

    fn prefer_ipv4_peer_manager() -> Arc<PeerManagerCore> {
        let runtime = PeerRuntimeConfig {
            core: CoreConfig::default(),
            network_identity: NetworkIdentity::default(),
            stun_info: StunInfo::default(),
            feature_flags: Default::default(),
            secure_mode: None,
            host_routing: HostRoutingPolicy::default(),
        };
        let mut peer_config = PortablePeerManagerConfig::new(runtime);
        peer_config.snapshot.flags.direct_conn_family = "prefer_ipv4".to_owned();
        let (packet_sender, packet_receiver) = crate::host::packet::host_packet_channel();
        std::mem::forget(packet_receiver);
        Arc::new(
            PeerManagerCore::new_portable_for_test(peer_config, packet_sender)
                .expect("build portable peer manager"),
        )
    }

    /// 一轮直连 = 一次外层退避循环迭代（R-c）：
    /// 拿 IP 列表跑 `try_direct_connect_with_ip_list`，跑到仍未满足偏好族就记
    /// 一次轮失败 —— 与 `try_direct_connect` 中的两个调用点逐一对应。
    async fn run_one_budget_round(
        data: &Arc<DirectConnectorData<TestDirectHost>>,
        dst_peer_id: PeerId,
        ip_list: GetIpListResponse,
    ) {
        let _ = data
            .try_direct_connect_with_ip_list(dst_peer_id, ip_list)
            .await;
        if !data.peer_satisfied(dst_peer_id) {
            data.family_round_failure(dst_peer_id);
        }
    }

    fn listeners_ip_list(listeners: &[&str]) -> GetIpListResponse {
        GetIpListResponse {
            listeners: listeners
                .iter()
                .map(|s| ProtoUrl::from(Url::parse(s).unwrap()))
                .collect(),
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn prefer_family_budget_gate_and_any_family_fallback_share_the_dial_path() {
        let peer_manager = prefer_ipv4_peer_manager();
        assert_eq!(
            peer_manager.direct_conn_family(),
            ConnFamilyPref::PreferIpv4
        );

        let data: Arc<DirectConnectorData<TestDirectHost>> = Arc::new(DirectConnectorData {
            peer_manager: peer_manager.clone(),
            host: Arc::new(TestDirectHost(Arc::new(TestHost::default()))),
            protected_tcp_ports: Arc::new(ProtectedTcpPortRegistry::default()),
            stun: Arc::new(TestStun),
            running_listeners: Arc::new(NoLocalListeners),
            dns: Arc::new(TestDns),
            protocol: Arc::new(TestUpgrader),
            options: DirectConnectorOptions {
                testing: true,
                ..Default::default()
            },
            listener_blacklist: ExpiringSet::default(),
            peer_blacklist: ExpiringSet::default(),
            family_budgets: std::sync::Mutex::new(std::collections::HashMap::new()),
            dial_probe: Arc::new(std::sync::Mutex::new(Vec::new())),
        });

        let dst_peer_id: PeerId = 7;
        // 该 peer 当前没有任何族的连接（兜底门的一部分条件）。
        assert!(!data.peer_satisfied_with(dst_peer_id, DirectSatisfaction::AnyFamily));

        // 一轮：偏好族（IPv4）listener 拨号失败 → 兜底门打开 → 另一族（IPv6）
        // listener 真正进入拨号级（不是停在 plan 函数上）。
        let ip_list = listeners_ip_list(&["tcp://127.0.0.1:9911", "tcp://[::1]:9911"]);
        run_one_budget_round(&data, dst_peer_id, ip_list).await;

        let probes = data.dial_probe.lock().unwrap().clone();
        assert!(
            probes.iter().any(|(peer, url, sat)| *peer == dst_peer_id
                && url.as_str() == "tcp://127.0.0.1:9911"
                && *sat == DirectSatisfaction::Preferred),
            "pass 1 (preferred family) must reach dial level: {probes:?}"
        );
        assert!(
            probes.iter().any(|(peer, url, sat)| *peer == dst_peer_id
                && url.as_str() == "tcp://[::1]:9911"
                && *sat == DirectSatisfaction::AnyFamily),
            "fallback pass must reach dial level with AnyFamily satisfaction: {probes:?}"
        );

        // 预算推进与兜底轮共存：连续 3 轮失败后窗口关闭（第 4 轮被预算放行判定拒绝）。
        for _ in 0..2 {
            run_one_budget_round(
                &data,
                dst_peer_id,
                listeners_ip_list(&["tcp://127.0.0.1:9911", "tcp://[::1]:9911"]),
            )
            .await;
        }
        assert!(!data.family_attempt_allowed(dst_peer_id));
        {
            let budgets = data.family_budgets.lock().unwrap();
            let state = budgets.get(&dst_peer_id).expect("budget recorded");
            assert_eq!(state.windows, 1);
            assert!(state.cooldown_until.is_some());
            assert!(!state.exhausted);
        }
        // 兜底拨号每一轮都真实发生（3 轮；每轮对该 URL 的内层重试会多次下探，
        // paused 时钟下 1000+2000+4000ms 即时推进 → 每轮每族 ≥4 次拨号，这里只断言
        // 每族 ≥3 次，即每轮至少一次真实拨号，不把内层重试次数焊死在断言里）。
        let probes = data.dial_probe.lock().unwrap().clone();
        assert!(
            probes
                .iter()
                .filter(|(_, _, sat)| *sat == DirectSatisfaction::AnyFamily)
                .count()
                >= 3
        );
        assert!(
            probes
                .iter()
                .filter(|(_, _, sat)| *sat == DirectSatisfaction::Preferred)
                .count()
                >= 3
        );
    }
}
