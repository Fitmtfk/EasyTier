use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use crossbeam_utils::atomic::AtomicCell;
use dashmap::{DashMap, DashSet};
use parking_lot::{Mutex, RwLock};

use tokio::{select, sync::mpsc};

use tracing::Instrument;

use super::family::{ConnFamily, ConnFamilyPref, conn_counts_as_family};
use super::peer_conn::{PeerConn, PeerConnId};
use crate::peers::{
    PacketRecvChan, PeerConnectionOrigin,
    context::{ArcPeerContext, PeerEvent},
    util::shrink_dashmap,
};
use crate::{
    config::PeerId,
    foundation::small_map_shard_amount,
    packet::ZCPacket,
    peers::error::Error,
    proto::{core_peer::peer::PeerConnInfo, peer_rpc::PeerIdentityType},
};
use tokio_util::task::AbortOnDropHandle;

type ArcPeerConn = Arc<PeerConn>;
type ConnMap = Arc<DashMap<PeerConnId, ArcPeerConn>>;

fn conn_latency_sort_key(latency_us: u64, is_hole_punched: bool) -> (bool, u64) {
    (is_hole_punched && latency_us == 0, latency_us)
}

/// 直连族偏好缓存的刷新周期：运行期改 `direct_conn_family` 后最多 5 秒生效（无需重启实例）。
const PREFER_FAMILY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

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

pub struct Peer {
    pub peer_node_id: PeerId,
    conns: ConnMap,
    context: ArcPeerContext,

    packet_recv_chan: PacketRecvChan,

    close_event_sender: mpsc::Sender<PeerConnId>,
    #[allow(dead_code)]
    close_event_listener: AbortOnDropHandle<()>,

    shutdown_notifier: Arc<tokio::sync::Notify>,

    default_conn: Arc<ArcSwapOption<PeerConn>>,
    default_conn_update_lock: Arc<Mutex<()>>,
    peer_identity_type: Arc<AtomicCell<Option<PeerIdentityType>>>,
    peer_public_key: Arc<RwLock<Option<Vec<u8>>>>,
    #[allow(dead_code)]
    default_conn_clear_task: AbortOnDropHandle<()>,

    /// 直连族偏好缓存；每 5 秒刷新一次（见 prefer_family_refresh_task）。
    /// 运行期改 `direct_conn_family` 后最多 5 秒生效，不需要重启实例。
    prefer_family: Arc<ArcSwap<ConnFamilyPref>>,
    /// 偏好缓存刷新任务句柄；存进 Peer，随 Peer drop 而 abort，不能丢。
    #[allow(dead_code)]
    prefer_family_refresh_task: AbortOnDropHandle<()>,
}

impl Peer {
    pub(crate) fn new(
        peer_node_id: PeerId,
        packet_recv_chan: PacketRecvChan,
        context: ArcPeerContext,
    ) -> Self {
        let conns: ConnMap = Arc::new(DashMap::with_shard_amount(small_map_shard_amount()));
        let (close_event_sender, mut close_event_receiver) = mpsc::channel(10);
        let shutdown_notifier = Arc::new(tokio::sync::Notify::new());
        let peer_identity_type = Arc::new(AtomicCell::new(None));
        let peer_identity_type_copy = peer_identity_type.clone();
        let peer_public_key = Arc::new(RwLock::new(None));
        let peer_public_key_copy = peer_public_key.clone();
        let default_conn = Arc::new(ArcSwapOption::empty());
        let default_conn_update_lock = Arc::new(Mutex::new(()));

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let context_copy = context.clone();
        let default_conn_copy = default_conn.clone();
        let default_conn_update_lock_copy = default_conn_update_lock.clone();
        let close_event_listener = AbortOnDropHandle::new(tokio::spawn(
            async move {
                loop {
                    select! {
                        ret = close_event_receiver.recv() => {
                            if ret.is_none() {
                                break;
                            }
                            let ret = ret.unwrap();
                            tracing::warn!(
                                ?peer_node_id,
                                ?ret,
                                "notified that peer conn is closed",
                            );

                            let removed_conn = {
                                let _update_guard = default_conn_update_lock_copy.lock();
                                let removed_conn = conns_copy.remove(&ret);
                                if let Some((_, conn)) = removed_conn.as_ref() {
                                    let cached_conn = default_conn_copy.load();
                                    if cached_conn
                                        .as_ref()
                                        .is_some_and(|cached| Arc::ptr_eq(cached, conn))
                                    {
                                        default_conn_copy.store(None);
                                    }
                                }
                                removed_conn
                            };

                            if let Some((_, conn)) = removed_conn {
                                context_copy.issue_event(PeerEvent::PeerConnRemoved(
                                    conn.get_conn_info(),
                                ));
                                shrink_dashmap(&conns_copy, Some(4));
                                if conns_copy.is_empty() {
                                    peer_identity_type_copy.store(None);
                                    *peer_public_key_copy.write() = None;
                                }
                            }
                        }

                        _ = shutdown_notifier_copy.notified() => {
                            close_event_receiver.close();
                            tracing::warn!(?peer_node_id, "peer close event listener notified");
                        }
                    }
                }
                tracing::info!("peer {} close event listener exit", peer_node_id);
            }
            .instrument(tracing::info_span!(
                "peer_close_event_listener",
                ?peer_node_id,
            )),
        ));

        let conns_copy = conns.clone();
        let default_conn_copy = default_conn.clone();
        let default_conn_clear_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                crate::foundation::time::sleep(std::time::Duration::from_secs(5)).await;
                if conns_copy.len() > 1 {
                    default_conn_copy.store(None);
                }
            }
        }));

        // 直连族偏好：初始值取当前旗标，之后每 5 秒刷新一次，
        // 运行期改开关最多 5 秒生效（不需要重启实例）。
        let prefer_family = Arc::new(ArcSwap::from_pointee(ConnFamilyPref::from_flag_str(
            &context.flags().direct_conn_family,
        )));
        let prefer_family_copy = prefer_family.clone();
        let prefer_context_copy = context.clone();
        let prefer_family_refresh_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                crate::foundation::time::sleep(PREFER_FAMILY_REFRESH_INTERVAL).await;
                let pref = ConnFamilyPref::from_flag_str(
                    &prefer_context_copy.flags().direct_conn_family,
                );
                prefer_family_copy.store(Arc::new(pref));
            }
        }));

        Peer {
            peer_node_id,
            conns,
            packet_recv_chan,
            context,

            close_event_sender,
            close_event_listener,

            shutdown_notifier,
            default_conn,
            default_conn_update_lock,
            peer_identity_type,
            peer_public_key,
            default_conn_clear_task,
            prefer_family,
            prefer_family_refresh_task,
        }
    }

    pub async fn add_peer_conn(&self, mut conn: PeerConn) -> Result<(), Error> {
        let conn_identity_type = conn.get_peer_identity_type();
        let peer_identity_type = self.peer_identity_type.load();
        if let Some(peer_identity_type) = peer_identity_type {
            if peer_identity_type != conn_identity_type {
                return Err(Error::SecretKeyError(format!(
                    "peer identity type mismatch. peer: {:?}, conn: {:?}",
                    peer_identity_type, conn_identity_type
                )));
            }
        } else {
            self.peer_identity_type.store(Some(conn_identity_type));
        }

        let close_notifier = conn.get_close_notifier();
        let conn_info = conn.get_conn_info();
        let conn_pubkey = conn_info.noise_remote_static_pubkey.clone();
        {
            let mut peer_pubkey = self.peer_public_key.write();
            if let Some(existing_pubkey) = peer_pubkey.as_ref() {
                if existing_pubkey != &conn_pubkey {
                    return Err(Error::SecretKeyError(format!(
                        "peer public key mismatch. peer_id: {}, existing_len: {}, new_len: {}",
                        self.peer_node_id,
                        existing_pubkey.len(),
                        conn_pubkey.len()
                    )));
                }
            } else {
                *peer_pubkey = Some(conn_pubkey);
            }
        }

        conn.start_recv_loop(self.packet_recv_chan.clone()).await;
        conn.start_pingpong();
        self.conns.insert(conn.get_conn_id(), Arc::new(conn));

        let close_event_sender = self.close_event_sender.clone();
        tokio::spawn(async move {
            let conn_id = close_notifier.get_conn_id();
            if let Some(mut waiter) = close_notifier.get_waiter().await {
                let _ = waiter.recv().await;
            }
            if let Err(e) = close_event_sender.send(conn_id).await {
                tracing::warn!(?conn_id, "failed to send close event: {}", e);
            }
        });

        self.context
            .issue_event(PeerEvent::PeerConnAdded(conn_info));
        Ok(())
    }

    fn select_conn(&self) -> Option<ArcPeerConn> {
        let _update_guard = self.default_conn_update_lock.lock();
        if let Some(conn) = self.default_conn.load_full() {
            return Some(conn);
        }

        // A zero latency on a hole-punched connection means the ping loop has not
        // confirmed liveness yet. Prefer any other connection, so a freshly admitted
        // hole-punched path cannot steal traffic before its first successful ping.
        //
        // `auto` 时走上游原路径（排序键完全一致），保证与上游逐位一致：
        // 上游是 min_by_key(conn_latency_sort_key)，并列时取首个最小值。
        // 只有显式指定偏好族时才把族分量插进排序键。
        let prefer = *self.prefer_family.load_full();
        let selected = if prefer == ConnFamilyPref::Auto {
            self.conns
                .iter()
                .min_by_key(|conn| {
                    conn_latency_sort_key(
                        conn.value().get_stats().latency_us,
                        conn.value().is_hole_punched(),
                    )
                })
                .map(|conn| conn.value().clone())
        } else {
            self.conns
                .iter()
                .min_by_key(|conn| {
                    let conn = conn.value();
                    conn_sort_key(
                        conn.get_stats().latency_us,
                        conn.is_hole_punched(),
                        conn.remote_host_family(),
                        prefer,
                    )
                })
                .map(|conn| conn.value().clone())
        };

        if let Some(conn) = selected.as_ref() {
            self.default_conn.store(Some(conn.clone()));
        }
        selected
    }

    /// 当前生效的直连族偏好（5 秒缓存；运行期改开关最多 5 秒内生效）。
    pub(crate) fn prefer_family(&self) -> ConnFamilyPref {
        *self.prefer_family.load_full()
    }

    pub async fn send_msg(&self, msg: ZCPacket) -> Result<(), Error> {
        let default_conn = self.default_conn.load();
        if let Some(conn) = default_conn.as_ref() {
            conn.send_msg(msg).await?;
            return Ok(());
        }
        drop(default_conn);

        let Some(conn) = self.select_conn() else {
            return Err(Error::PeerNoConnectionError(self.peer_node_id));
        };
        conn.send_msg(msg).await?;

        Ok(())
    }

    pub async fn close_peer_conn(&self, conn_id: &PeerConnId) -> Result<(), Error> {
        let has_key = self.conns.contains_key(conn_id);
        if !has_key {
            return Err(Error::NotFound);
        }
        self.close_event_sender.send(*conn_id).await.unwrap();
        Ok(())
    }

    pub async fn list_peer_conns(&self) -> Vec<PeerConnInfo> {
        let mut conns = vec![];
        for conn in self.conns.iter() {
            // do not lock here, otherwise it will cause dashmap deadlock
            conns.push(conn.clone());
        }

        let mut ret = Vec::new();
        for conn in conns {
            let info = conn.get_conn_info();
            if !info.is_closed {
                ret.push(info);
            } else {
                let conn_id = info.conn_id.parse().unwrap();
                let _ = self.close_peer_conn(&conn_id).await;
            }
        }
        ret
    }

    pub fn has_live_conns(&self) -> bool {
        self.conns.iter().any(|entry| !entry.value().is_closed())
    }

    pub fn has_directly_connected_conn(&self) -> bool {
        self.conns
            .iter()
            .any(|entry| !entry.value().is_closed() && !entry.value().is_hole_punched())
    }

    /// 是否存在未关闭、且族为 family 的连接。
    /// 与 has_directly_connected_conn 不同：**包含打洞得到的连接**。
    pub fn has_conn_of_family(&self, family: ConnFamily) -> bool {
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            conn_counts_as_family(conn.remote_host_family(), conn.is_closed(), family)
        })
    }

    /// 是否存在未关闭、且来源为 origin 的连接。
    /// 打洞层用它判断「本传输是否已为该 peer 产出过连接」。
    pub(crate) fn has_conn_with_origin(&self, origin: PeerConnectionOrigin) -> bool {
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            !conn.is_closed() && conn.origin() == origin
        })
    }

    pub(crate) fn has_direct_attached_conn(&self) -> bool {
        self.conns.iter().any(|entry| {
            let conn = entry.value();
            !conn.is_closed() && !conn.is_hole_punched() && conn.is_attached()
        })
    }

    pub fn get_directly_connections(&self) -> DashSet<uuid::Uuid> {
        self.conns
            .iter()
            .filter(|entry| !(entry.value()).is_hole_punched())
            .map(|entry| (entry.value()).get_conn_id())
            .collect()
    }

    pub fn get_default_conn_id(&self) -> PeerConnId {
        self.default_conn
            .load()
            .as_ref()
            .map(|conn| conn.get_conn_id())
            .unwrap_or_default()
    }

    pub fn get_peer_identity_type(&self) -> Option<PeerIdentityType> {
        self.peer_identity_type.load()
    }

    pub fn get_peer_public_key(&self) -> Option<Vec<u8>> {
        self.peer_public_key.read().clone()
    }
}

// pritn on drop
impl Drop for Peer {
    fn drop(&mut self) {
        self.conns.retain(|_, conn| {
            self.context
                .issue_event(PeerEvent::PeerConnRemoved(conn.get_conn_info()));
            false
        });
        self.shutdown_notifier.notify_one();
        tracing::info!("peer {} drop", self.peer_node_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use easytier_proto::common::Flags;

    use super::conn_latency_sort_key;
    use super::{ConnFamily, ConnFamilyPref, Peer, conn_counts_as_family, conn_sort_key};
    use crate::peers::conn::peer_conn::PeerConn;
    use crate::peers::conn::peer_session::PeerSessionStore;
    use crate::peers::context::{ArcPeerContext, NetworkIdentity, PeerContext};
    use crate::peers::test_support::NoopPeerContext;
    use crate::peers::{PeerConnectionOrigin, create_packet_recv_chan};
    use crate::proto::common::TunnelInfo;
    use crate::tunnel::ring::{RingTunnel, create_ring_socket_pair};

    fn tunnel_with_remote_addr(remote_addr: &str) -> Box<dyn crate::tunnel::Tunnel> {
        let (socket, _peer_socket) = create_ring_socket_pair(16);
        let info = TunnelInfo {
            tunnel_type: "ring".to_owned(),
            local_addr: None,
            remote_addr: Some(crate::proto::common::Url {
                url: remote_addr.to_owned(),
            }),
            resolved_remote_addr: None,
        };
        Box::new(RingTunnel::new(socket, Some(info)))
    }

    /// 构造一条指定对端地址、指定来源的连接。
    fn conn_with_remote_addr(
        context: &ArcPeerContext,
        remote_addr: &str,
        origin: PeerConnectionOrigin,
    ) -> PeerConn {
        PeerConn::new_with_peer_id_hint_and_origin(
            1,
            context.clone(),
            tunnel_with_remote_addr(remote_addr),
            None,
            Arc::new(PeerSessionStore::new()),
            origin,
        )
    }

    /// 带指定 direct_conn_family 旗标的 PeerContext。
    fn context_with_prefer(flag: &str) -> ArcPeerContext {
        let mut flags = Flags::default();
        flags.direct_conn_family = flag.to_owned();
        Arc::new(NoopPeerContext::default().with_flags(flags))
    }

    /// 可运行期改旗标的 PeerContext（用于验证 5 秒刷新）。
    struct MutableFlagsContext {
        base: NoopPeerContext,
        flags: Arc<ArcSwap<Flags>>,
    }

    impl PeerContext for MutableFlagsContext {
        fn network_identity(&self) -> NetworkIdentity {
            self.base.network_identity()
        }

        fn flags(&self) -> Flags {
            self.flags.load_full().as_ref().clone()
        }
    }

    #[tokio::test]
    async fn family_predicate_counts_hole_punched_conn() {
        let context = Arc::new(NoopPeerContext::default());
        let conn = PeerConn::new_with_peer_id_hint_and_origin(
            1,
            context.clone(),
            tunnel_with_remote_addr("tcp://1.2.3.4:11010"),
            None,
            Arc::new(PeerSessionStore::new()),
            PeerConnectionOrigin::UdpHolePunch,
        );
        assert!(conn.is_hole_punched(), "conn should be hole punched");
        assert_eq!(conn.remote_host_family(), ConnFamily::Ipv4);

        let (packet_recv_chan, _recv_chan) = create_packet_recv_chan();
        let peer = Peer::new(2, packet_recv_chan, context);
        let conn_id = conn.get_conn_id();
        peer.conns.insert(conn_id, Arc::new(conn));

        // 打洞连接不算「直连」，但必须算作「已有该族的连接」。
        assert!(!peer.has_directly_connected_conn());
        assert!(peer.has_conn_of_family(ConnFamily::Ipv4));
        assert!(!peer.has_conn_of_family(ConnFamily::Ipv6));
        assert!(!peer.has_conn_of_family(ConnFamily::Unknown));

        // 未握手的连接没有 info，get_conn_info() 会 panic（上游既有行为），
        // 因此先摘掉连接再让 Peer 析构。
        peer.conns.remove(&conn_id);
    }

    #[tokio::test]
    async fn has_conn_with_origin_distinguishes_origin() {
        let context: ArcPeerContext = Arc::new(NoopPeerContext::default());
        let punched = conn_with_remote_addr(
            &context,
            "tcp://1.2.3.4:11010",
            PeerConnectionOrigin::UdpHolePunch,
        );
        let direct = conn_with_remote_addr(
            &context,
            "tcp://[2001:db8::1]:11010",
            PeerConnectionOrigin::Direct,
        );

        let (packet_recv_chan, _recv_chan) = create_packet_recv_chan();
        let peer = Peer::new(2, packet_recv_chan, context);
        let punched_conn_id = punched.get_conn_id();
        let direct_conn_id = direct.get_conn_id();
        peer.conns.insert(punched_conn_id, Arc::new(punched));
        peer.conns.insert(direct_conn_id, Arc::new(direct));

        assert!(peer.has_conn_with_origin(PeerConnectionOrigin::UdpHolePunch));
        assert!(peer.has_conn_with_origin(PeerConnectionOrigin::Direct));
        // 别的打洞传输 / 其他来源不算。
        assert!(!peer.has_conn_with_origin(PeerConnectionOrigin::TcpHolePunch));
        assert!(!peer.has_conn_with_origin(PeerConnectionOrigin::Manual));

        // 未握手的连接没有 info，get_conn_info() 会 panic（上游既有行为），
        // 因此先摘掉连接再让 Peer 析构。
        peer.conns.remove(&punched_conn_id);
        peer.conns.remove(&direct_conn_id);
    }

    #[test]
    fn family_predicate_excludes_closed_and_unknown() {
        assert!(conn_counts_as_family(
            ConnFamily::Ipv4,
            false,
            ConnFamily::Ipv4
        ));
        assert!(!conn_counts_as_family(
            ConnFamily::Ipv4,
            true,
            ConnFamily::Ipv4
        ));
        assert!(!conn_counts_as_family(
            ConnFamily::Unknown,
            false,
            ConnFamily::Ipv4
        ));
        assert!(!conn_counts_as_family(
            ConnFamily::Unknown,
            false,
            ConnFamily::Ipv6
        ));
    }

    #[test]
    fn measured_relay_precedes_unverified_hole_punch_path() {
        assert!(conn_latency_sort_key(20_000, false) < conn_latency_sort_key(0, true));
    }

    #[test]
    fn unmeasured_regular_connection_keeps_existing_priority() {
        assert!(conn_latency_sort_key(0, false) < conn_latency_sort_key(20_000, false));
    }

    #[test]
    fn verified_lower_latency_path_can_be_preferred() {
        assert!(conn_latency_sort_key(5_000, true) < conn_latency_sort_key(20_000, false));
    }

    // ---- 族偏好排序键（Task 3）----

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

    #[tokio::test]
    async fn auto_selection_matches_upstream_selection_including_ties() {
        let context = context_with_prefer("auto");
        let (packet_recv_chan, _recv_chan) = create_packet_recv_chan();
        let peer = Peer::new(2, packet_recv_chan, context.clone());

        // 三条连接：延迟都是 0、都不是打洞 → 上游键全部并列，
        // min_by_key 取「首个」最小值，并列时保持先出现者。
        let v4 = conn_with_remote_addr(&context, "tcp://1.2.3.4:11010", PeerConnectionOrigin::Direct);
        let v6 = conn_with_remote_addr(&context, "tcp://[2001:db8::1]:11010", PeerConnectionOrigin::Direct);
        let unknown = conn_with_remote_addr(&context, "tcp://example.com:11010", PeerConnectionOrigin::Direct);
        peer.conns.insert(v4.get_conn_id(), Arc::new(v4));
        peer.conns.insert(v6.get_conn_id(), Arc::new(v6));
        peer.conns.insert(unknown.get_conn_id(), Arc::new(unknown));

        // 上游算法：只按 (unconfirmed, latency) 选。
        let upstream_pick = peer
            .conns
            .iter()
            .min_by_key(|conn| {
                conn_latency_sort_key(
                    conn.value().get_stats().latency_us,
                    conn.value().is_hole_punched(),
                )
            })
            .map(|conn| conn.value().get_conn_id());
        assert!(upstream_pick.is_some());

        // 新键在 auto 下（族分量恒 0）必须选出同一个连接。
        let new_pick = peer
            .conns
            .iter()
            .min_by_key(|conn| {
                let conn = conn.value();
                conn_sort_key(
                    conn.get_stats().latency_us,
                    conn.is_hole_punched(),
                    conn.remote_host_family(),
                    ConnFamilyPref::Auto,
                )
            })
            .map(|conn| conn.value().get_conn_id());
        assert_eq!(upstream_pick, new_pick, "auto 下新键必须与上游选出同一个连接");

        // select_conn 实际选中的也必须与上游一致。
        assert_eq!(
            peer.select_conn().map(|conn| conn.get_conn_id()),
            upstream_pick
        );

        // 未握手的连接没有 info，get_conn_info() 会 panic（上游既有行为），
        // 因此先摘掉连接再让 Peer 析构。
        peer.conns.clear();
    }

    #[tokio::test]
    async fn preferred_family_selection_prefers_preferred_family_conn() {
        let context = context_with_prefer("prefer_ipv4");
        let (packet_recv_chan, _recv_chan) = create_packet_recv_chan();
        let peer = Peer::new(2, packet_recv_chan, context.clone());
        assert_eq!(peer.prefer_family(), ConnFamilyPref::PreferIpv4);

        // 三条连接延迟都是 0、都不是打洞 → 上游只会按迭代顺序取首个；
        // prefer_ipv4 下只有 v4 的族分量是 0，必须选 v4。
        let v4 = conn_with_remote_addr(&context, "tcp://1.2.3.4:11010", PeerConnectionOrigin::Direct);
        let v6 = conn_with_remote_addr(&context, "tcp://[2001:db8::1]:11010", PeerConnectionOrigin::Direct);
        let unknown = conn_with_remote_addr(&context, "tcp://example.com:11010", PeerConnectionOrigin::Direct);
        let v4_id = v4.get_conn_id();
        peer.conns.insert(v4.get_conn_id(), Arc::new(v4));
        peer.conns.insert(v6.get_conn_id(), Arc::new(v6));
        peer.conns.insert(unknown.get_conn_id(), Arc::new(unknown));

        let selected = peer.select_conn().expect("should select a conn");
        assert_eq!(selected.get_conn_id(), v4_id);

        peer.conns.clear();
    }

    #[tokio::test]
    async fn unknown_family_outranks_non_preferred_family_in_selection() {
        let context = context_with_prefer("prefer_ipv4");
        let (packet_recv_chan, _recv_chan) = create_packet_recv_chan();
        let peer = Peer::new(2, packet_recv_chan, context.clone());

        // 没有 v4 连接：Unknown（族分量 1）必须胜过另一族（族分量 2）。
        let v6 = conn_with_remote_addr(&context, "tcp://[2001:db8::1]:11010", PeerConnectionOrigin::Direct);
        let unknown = conn_with_remote_addr(&context, "tcp://example.com:11010", PeerConnectionOrigin::Direct);
        let unknown_id = unknown.get_conn_id();
        peer.conns.insert(v6.get_conn_id(), Arc::new(v6));
        peer.conns.insert(unknown.get_conn_id(), Arc::new(unknown));

        let selected = peer.select_conn().expect("should select a conn");
        assert_eq!(selected.get_conn_id(), unknown_id);

        peer.conns.clear();
    }

    #[tokio::test]
    async fn prefer_family_cache_refreshes_within_five_seconds() {
        let flags = Arc::new(ArcSwap::from_pointee(Flags::default()));
        let context: ArcPeerContext = Arc::new(MutableFlagsContext {
            base: NoopPeerContext::default(),
            flags: flags.clone(),
        });
        let (packet_recv_chan, _recv_chan) = create_packet_recv_chan();
        let peer = Peer::new(2, packet_recv_chan, context.clone());
        assert_eq!(peer.prefer_family(), ConnFamilyPref::Auto);

        // 运行期改开关：最多 5 秒内生效，不需要重启实例。
        let mut new_flags = flags.load_full().as_ref().clone();
        new_flags.direct_conn_family = "prefer_ipv4".to_owned();
        flags.store(Arc::new(new_flags));

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while peer.prefer_family() != ConnFamilyPref::PreferIpv4 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "prefer family cache should refresh within 5s, got {:?}",
                peer.prefer_family()
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        peer.conns.clear();
    }
}
