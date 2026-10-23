use std::{
    net::{IpAddr, Ipv6Addr},
    sync::Arc,
};

use async_trait::async_trait;
use quanta::Instant;

use crate::{
    config::{P2pPolicyFlags, PeerId},
    foundation::task::ExternalTaskSignal,
    peers::{
        PeerConnectionOrigin,
        conn::family::{ConnFamily, ConnFamilyPref},
        peer_manager::PeerManagerCore,
    },
    proto::{
        common::NatType,
        peer_rpc::{
            TcpHolePunchRpc, TcpHolePunchRpcClientFactory, UdpHolePunchRpc,
            UdpHolePunchRpcClientFactory,
        },
        rpc_types::{controller::BaseController, handler::Handler},
    },
    tunnel::Tunnel,
};

use super::{
    HolePunchRpcRegistry, HolePunchTunnelSink,
    tcp::{TcpHolePunchPeerSource, TcpPunchCandidate},
    udp::{UdpHolePunchPeerSource, UdpHolePunchRpcSource, UdpPunchCandidate},
};

/// 该 peer 是否已满足「不需要再打洞」的条件。
///
/// - `Auto`：保持上游语义（已有任意连接就不打洞）。
/// - `prefer_ipv4` / `prefer_ipv6`：已有偏好族连接才不打洞。
///   例外：UDP 对称打洞只能产出 IPv4 路径，`prefer_ipv6` 下保持原样。
///   连接属于哪一族不由本地决定（UDP 固定 V4，TCP 取决于对端映射地址），
///   因此按**传输**收敛：本传输已产出过非偏好族连接 → 已尽力，不再重复打洞；
///   别的传输（尤其 `prefer_ipv4` 下的 UDP）仍可继续打洞。
///
/// 判定用闭包注入（不直接依赖 `PeerMap`），便于测试：
/// - `has_conn`：该 peer 是否已有**偏好族**的连接（含打洞得到的连接）。
/// - `has_any`：该 peer 是否已有**任意**连接（上游 `has_peer` 语义）。
/// - `has_punched`：该 peer 是否已有**本传输**打洞来源的连接（未关闭）。
pub(crate) fn has_satisfied_punch_condition(
    has_conn: impl Fn(PeerId) -> bool,
    has_any: impl Fn(PeerId) -> bool,
    has_punched: impl Fn(PeerId) -> bool,
    peer_id: PeerId,
    prefer: ConnFamilyPref,
    udp_only_ipv4: bool,
) -> bool {
    match prefer.preferred_family() {
        // UDP 对称打洞产不出 IPv6 路径，prefer_ipv6 下保持上游语义，
        // 否则会永远重复发起打洞。
        Some(_) if !(udp_only_ipv4 && prefer == ConnFamilyPref::PreferIpv6) => {
            if has_conn(peer_id) {
                true // 偏好族已连通
            } else {
                has_punched(peer_id) // 本传输已尽力，不再重复打洞
            }
        }
        _ => has_any(peer_id), // auto / prefer_ipv6+UDP：上游语义
    }
}

#[async_trait]
impl UdpHolePunchPeerSource for PeerManagerCore {
    fn local_peer_id(&self) -> PeerId {
        PeerManagerCore::my_peer_id(self)
    }

    fn p2p_policy_flags(&self) -> P2pPolicyFlags {
        PeerManagerCore::p2p_policy_flags(self)
    }

    async fn candidates(&self) -> Vec<UdpPunchCandidate> {
        let now = Instant::now();
        let peer_map = self.get_peer_map();
        let prefer = PeerManagerCore::direct_conn_family(self);
        // has_conn 只会在存在偏好族时被调用（见 has_satisfied_punch_condition）。
        let has_conn = |id: PeerId| match prefer.preferred_family() {
            Some(family) => peer_map.has_conn_of_family(id, family),
            None => false,
        };
        let has_any = |id: PeerId| peer_map.has_peer(id);
        let has_punched =
            |id: PeerId| peer_map.has_conn_with_origin(id, PeerConnectionOrigin::UdpHolePunch);
        self.list_route_snapshots()
            .await
            .into_iter()
            .filter_map(|route| {
                let udp_nat_type = route
                    .stun_info
                    .as_ref()
                    .map(|info| info.udp_nat_type)
                    .unwrap_or_default();
                let Ok(udp_nat_type) = crate::proto::common::NatType::try_from(udp_nat_type) else {
                    return None;
                };
                Some(UdpPunchCandidate {
                    peer_id: route.peer_id,
                    udp_nat_type,
                    feature_flag: route.feature_flag,
                    has_direct_connection: has_satisfied_punch_condition(
                        &has_conn,
                        &has_any,
                        &has_punched,
                        route.peer_id,
                        prefer,
                        true, // UDP 对称打洞只能产出 IPv4 路径
                    ),
                    has_recent_traffic: self.has_recent_traffic(route.peer_id, now),
                })
            })
            .collect()
    }

    fn p2p_demand_notify(&self) -> Arc<ExternalTaskSignal> {
        PeerManagerCore::p2p_demand_notify(self)
    }

    fn is_local_virtual_ip(&self, ip: &IpAddr) -> bool {
        PeerManagerCore::is_local_virtual_ip(self, ip)
    }

    async fn is_easytier_managed_ipv6(&self, ip: &Ipv6Addr) -> bool {
        PeerManagerCore::is_easytier_managed_ipv6(self, ip).await
    }
}

impl UdpHolePunchRpcSource for PeerManagerCore {
    fn local_peer_id(&self) -> PeerId {
        PeerManagerCore::my_peer_id(self)
    }

    fn rpc_stub(
        &self,
        dst_peer_id: PeerId,
    ) -> Box<dyn UdpHolePunchRpc<Controller = BaseController> + Send + Sync + 'static> {
        PeerManagerCore::get_peer_rpc_mgr(self)
            .rpc_client()
            .scoped_client::<UdpHolePunchRpcClientFactory<BaseController>>(
                PeerManagerCore::my_peer_id(self),
                dst_peer_id,
                PeerManagerCore::network_name(self).to_owned(),
            )
    }
}

#[async_trait]
impl HolePunchTunnelSink for PeerManagerCore {
    async fn add_client_tunnel(
        &self,
        tunnel: Box<dyn Tunnel>,
        origin: PeerConnectionOrigin,
    ) -> anyhow::Result<()> {
        self.add_client_tunnel_with_peer_id_hint(tunnel, origin, None)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    }

    async fn add_server_tunnel(
        &self,
        tunnel: Box<dyn Tunnel>,
        origin: PeerConnectionOrigin,
    ) -> anyhow::Result<()> {
        self.add_tunnel_as_server_with_origin(tunnel, origin)
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
    }
}

#[async_trait]
impl TcpHolePunchPeerSource for PeerManagerCore {
    fn local_peer_id(&self) -> PeerId {
        PeerManagerCore::my_peer_id(self)
    }

    fn p2p_policy_flags(&self) -> P2pPolicyFlags {
        PeerManagerCore::p2p_policy_flags(self)
    }

    fn tcp_hole_punching_disabled(&self) -> bool {
        PeerManagerCore::tcp_hole_punching_disabled(self)
    }

    fn p2p_demand_notify(&self) -> Arc<ExternalTaskSignal> {
        PeerManagerCore::p2p_demand_notify(self)
    }

    async fn candidates(&self) -> Vec<TcpPunchCandidate> {
        let now = Instant::now();
        let peer_map = self.get_peer_map();
        let prefer = PeerManagerCore::direct_conn_family(self);
        // has_conn 只会在存在偏好族时被调用（见 has_satisfied_punch_condition）。
        let has_conn = |id: PeerId| match prefer.preferred_family() {
            Some(family) => peer_map.has_conn_of_family(id, family),
            None => false,
        };
        let has_any = |id: PeerId| peer_map.has_peer(id);
        let has_punched =
            |id: PeerId| peer_map.has_conn_with_origin(id, PeerConnectionOrigin::TcpHolePunch);
        self.list_route_snapshots()
            .await
            .into_iter()
            .map(|route| TcpPunchCandidate {
                peer_id: route.peer_id,
                tcp_nat_type: route
                    .stun_info
                    .as_ref()
                    .map(|info| info.tcp_nat_type)
                    .and_then(|nat_type| NatType::try_from(nat_type).ok())
                    .unwrap_or(NatType::Unknown),
                feature_flag: route.feature_flag,
                has_direct_connection: has_satisfied_punch_condition(
                    &has_conn,
                    &has_any,
                    &has_punched,
                    route.peer_id,
                    prefer,
                    false, // TCP 对称打洞两族都支持
                ),
                has_recent_traffic: self.has_recent_traffic(route.peer_id, now),
            })
            .collect()
    }

    fn rpc_stub(
        &self,
        dst_peer_id: PeerId,
    ) -> Box<dyn TcpHolePunchRpc<Controller = BaseController> + Send + Sync + 'static> {
        PeerManagerCore::get_peer_rpc_mgr(self)
            .rpc_client()
            .scoped_client::<TcpHolePunchRpcClientFactory<BaseController>>(
                PeerManagerCore::my_peer_id(self),
                dst_peer_id,
                PeerManagerCore::network_name(self).to_owned(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peers::conn::family::{ConnFamily, ConnFamilyPref};

    const PEER: PeerId = 42;

    /// 模拟一个 peer 的连接集合：peer_families 列出它已有的连接族，
    /// punched 表示「本传输已为该 peer 产出过连接（未关闭）」。
    /// 接线方式与生产代码一致：has_conn 按偏好族判定，has_any 判定「任意连接」，
    /// has_punched 按本传输的打洞来源判定。
    fn punch_satisfied(
        prefer: ConnFamilyPref,
        udp_only_ipv4: bool,
        peer_families: &[ConnFamily],
        punched: bool,
    ) -> bool {
        let preferred = prefer.preferred_family();
        has_satisfied_punch_condition(
            |_id| match preferred {
                Some(family) => peer_families.contains(&family),
                None => false,
            },
            |_id| !peer_families.is_empty(),
            |_id| punched,
            PEER,
            prefer,
            udp_only_ipv4,
        )
    }

    #[test]
    fn auto_keeps_upstream_skip_semantics() {
        // Auto：上游语义 —— 已有任意连接就不打洞，没有连接才打洞。
        assert!(!punch_satisfied(ConnFamilyPref::Auto, true, &[], false));
        assert!(!punch_satisfied(ConnFamilyPref::Auto, false, &[], false));
        assert!(punch_satisfied(
            ConnFamilyPref::Auto,
            true,
            &[ConnFamily::Ipv6],
            false
        ));
        assert!(punch_satisfied(
            ConnFamilyPref::Auto,
            false,
            &[ConnFamily::Ipv4],
            false
        ));
    }

    #[test]
    fn prefer_ipv4_keeps_punching_when_only_ipv6_connected() {
        // 只连上了 IPv6：偏好族没满足 → 继续发起打洞。
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            true,
            &[ConnFamily::Ipv6],
            false
        ));
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            false,
            &[ConnFamily::Ipv6],
            false
        ));
    }

    #[test]
    fn prefer_ipv4_stops_punching_after_ipv4_conn() {
        // UDP 打洞成功后得到的正是 IPv4 连接 → 偏好族已满足，不再重复发起。
        assert!(punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            true,
            &[ConnFamily::Ipv4],
            false
        ));
        assert!(punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            false,
            &[ConnFamily::Ipv4],
            false
        ));
        // 两族并存时同样满足。
        assert!(punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            true,
            &[ConnFamily::Ipv4, ConnFamily::Ipv6],
            false
        ));
    }

    #[test]
    fn prefer_ipv6_does_not_count_ipv4_conn_as_ipv6() {
        // TCP 对称打洞两族都支持：IPv4 连接不算「已有 IPv6 连接」→ 继续打洞。
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv6,
            false,
            &[ConnFamily::Ipv4],
            false
        ));
        assert!(punch_satisfied(
            ConnFamilyPref::PreferIpv6,
            false,
            &[ConnFamily::Ipv6],
            false
        ));
    }

    #[test]
    fn prefer_ipv6_udp_punch_keeps_upstream_semantics() {
        // 唯一例外：UDP 对称打洞产不出 IPv6 路径，prefer_ipv6 下保持上游语义，
        // 否则会永远重复打洞。
        assert!(punch_satisfied(
            ConnFamilyPref::PreferIpv6,
            true,
            &[ConnFamily::Ipv4],
            false
        ));
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv6,
            true,
            &[],
            false
        ));
    }

    #[test]
    fn prefer_ipv4_direct_ipv6_conn_does_not_block_punching() {
        // 用户主场景：已有 Direct 来源的 IPv6 连接时，仍要打洞
        // （Direct 来源不是打洞来源，TCP 与 UDP 都不受它阻塞）。
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            false,
            &[ConnFamily::Ipv6],
            false
        ));
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            true,
            &[ConnFamily::Ipv6],
            false
        ));
    }

    #[test]
    fn prefer_ipv4_tcp_transport_converges_after_non_preferred_conn() {
        // TCP 对称打洞连的是对端映射地址，可能产出非偏好族连接：
        // prefer_ipv4 下产出 IPv6 → TCP 该传输已尽力，不再重复打洞；
        // 但 UDP 能产出 IPv4，必须继续打洞。
        assert!(punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            false,
            &[ConnFamily::Ipv6],
            true
        ));
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv4,
            true,
            &[ConnFamily::Ipv6],
            false
        ));
    }

    #[test]
    fn prefer_ipv6_tcp_transport_converges_after_non_preferred_conn() {
        // prefer_ipv6 + TCP：对端映射地址是 IPv4 → 打洞产出 IPv4 连接。
        // 该传输已尽力 → 不再重复打洞（否则连接数会持续增长）。
        assert!(punch_satisfied(
            ConnFamilyPref::PreferIpv6,
            false,
            &[ConnFamily::Ipv4],
            true
        ));
        // 本传输还没产出过连接 → 继续打洞。
        assert!(!punch_satisfied(
            ConnFamilyPref::PreferIpv6,
            false,
            &[ConnFamily::Ipv4],
            false
        ));
    }
}

#[async_trait]
impl HolePunchRpcRegistry for PeerManagerCore {
    fn register_rpc_service<H>(&self, service: H)
    where
        H: Handler<Controller = BaseController>,
    {
        PeerManagerCore::get_peer_rpc_mgr(self)
            .rpc_server()
            .registry()
            .register(service, PeerManagerCore::network_name(self));
    }

    fn unregister_rpc_service<H>(&self, service: H)
    where
        H: Handler<Controller = BaseController>,
    {
        PeerManagerCore::get_peer_rpc_mgr(self)
            .rpc_server()
            .registry()
            .unregister(service, PeerManagerCore::network_name(self));
    }
}
