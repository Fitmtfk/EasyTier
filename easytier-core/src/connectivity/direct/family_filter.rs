//! 直连连接器的「族感知」过滤与判定。
//!
//! 直连族偏好由 `ConnFamilyPref` 表达（`auto` / `prefer_ipv4` / `prefer_ipv6`）：
//! - `auto`：与上游逐位一致，不做任何族过滤，判定沿用「已有直连连接就放弃」。
//! - `prefer_ipv4` / `prefer_ipv6`：只把「偏好族已连通」当作放弃直连的理由，
//!   并且只尝试偏好族的 listener（域名 listener 放行，由 `IpVersion` 限定控族）。

use std::net::IpAddr;

use url::Url;

use crate::{
    peers::conn::family::{ConnFamily, ConnFamilyPref},
    socket::IpVersion,
};

/// listener URL 的字面量地址族；域名或无法解析的 URL 返回 None。
fn literal_family(listener_url: &str) -> Option<ConnFamily> {
    let url = Url::parse(listener_url).ok()?;
    match url.host()? {
        url::Host::Ipv4(_) => Some(ConnFamily::Ipv4),
        url::Host::Ipv6(_) => Some(ConnFamily::Ipv6),
        // url crate 对非特殊 scheme（udp/tcp/...）按 opaque host 解析，
        // "1.2.3.4" 会落在 Host::Domain 里，必须自己再判一次字面量族，
        // 否则 IPv4 listener 会被当成域名放行（族过滤形同虚设）。
        url::Host::Domain(domain) => match domain.parse::<IpAddr>() {
            Ok(IpAddr::V4(_)) => Some(ConnFamily::Ipv4),
            Ok(IpAddr::V6(_)) => Some(ConnFamily::Ipv6),
            Err(_) => None,
        },
    }
}

/// listener URL 是否允许在当前偏好下尝试。
///
/// - `auto`：全部允许（与上游一致）。
/// - 字面量 IP：只允许偏好族（`0.0.0.0` 属 V4、`[::]` 属 V6），
///   偏好族自己的 unspecified 展开源因此被保留。
/// - 域名：不在此层过滤（由 `IpVersion` 限定控族）。
pub fn listener_matches_prefer_family(listener_url: &str, prefer: ConnFamilyPref) -> bool {
    let Some(preferred) = prefer.preferred_family() else {
        // Auto：与上游逐位一致，全部放行。
        return true;
    };
    match literal_family(listener_url) {
        // 域名（或无法判定）：不在此层过滤，由 IpVersion 限定控族。
        None => true,
        Some(family) => family == preferred,
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

/// 直连需求是否已被满足。
///
/// - `auto`：完全沿用上游语义（「已有直连连接」即放弃）。
/// - `prefer_*`：只有「已有偏好族连接」才放弃；另一族（例如 IPv6）的连接
///   不影响继续尝试偏好族。判定走 `has_conn_of_family`（**含打洞连接**），
///   不用 `has_directly_connected_conn`（它把打洞连接排除在外）。
pub fn peer_satisfied_by(
    prefer: ConnFamilyPref,
    has_conn_of_family: impl Fn(ConnFamily) -> bool,
    has_directly_connected: impl Fn() -> bool,
) -> bool {
    match prefer.preferred_family() {
        Some(family) => has_conn_of_family(family),
        None => has_directly_connected(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn auto_keeps_all_listeners() {
        for url in [
            "udp://0.0.0.0:11010",
            "udp://[::]:11010",
            "tcp://1.2.3.4:11010",
            "tcp://[2001:db8::1]:11010",
            "tcp://example.com:11010",
        ] {
            assert!(
                listener_matches_prefer_family(url, ConnFamilyPref::Auto),
                "{url}"
            );
        }
    }

    #[test]
    fn prefer_ipv4_keeps_ipv4_and_unspecified_ipv4_only() {
        assert!(listener_matches_prefer_family(
            "udp://0.0.0.0:11010",
            ConnFamilyPref::PreferIpv4
        ));
        assert!(listener_matches_prefer_family(
            "tcp://1.2.3.4:11010",
            ConnFamilyPref::PreferIpv4
        ));
        assert!(!listener_matches_prefer_family(
            "udp://[::]:11010",
            ConnFamilyPref::PreferIpv4
        ));
        assert!(!listener_matches_prefer_family(
            "tcp://[2001:db8::1]:11010",
            ConnFamilyPref::PreferIpv4
        ));
        // 域名不在此层过滤
        assert!(listener_matches_prefer_family(
            "tcp://example.com:11010",
            ConnFamilyPref::PreferIpv4
        ));
    }

    #[test]
    fn prefer_ipv6_keeps_ipv6_and_unspecified_ipv6_only() {
        assert!(listener_matches_prefer_family(
            "udp://[::]:11010",
            ConnFamilyPref::PreferIpv6
        ));
        assert!(listener_matches_prefer_family(
            "tcp://[2001:db8::1]:11010",
            ConnFamilyPref::PreferIpv6
        ));
        assert!(!listener_matches_prefer_family(
            "udp://0.0.0.0:11010",
            ConnFamilyPref::PreferIpv6
        ));
        assert!(!listener_matches_prefer_family(
            "tcp://1.2.3.4:11010",
            ConnFamilyPref::PreferIpv6
        ));
        assert!(listener_matches_prefer_family(
            "tcp://example.com:11010",
            ConnFamilyPref::PreferIpv6
        ));
    }

    #[test]
    fn ipv4_literal_host_of_opaque_scheme_is_detected() {
        // url crate 对非特殊 scheme（udp/tcp）按 opaque host 解析，
        // "0.0.0.0" 会落在 Host::Domain 里，必须自己再判一次字面量族，
        // 否则 prefer_ipv6 下 IPv4 的展开源会被当成域名放行。
        assert!(matches!(
            Url::parse("udp://0.0.0.0:11010").unwrap().host(),
            Some(url::Host::Domain(_))
        ));
        assert!(!listener_matches_prefer_family(
            "udp://0.0.0.0:11010",
            ConnFamilyPref::PreferIpv6
        ));
        assert!(!listener_matches_prefer_family(
            "tcp://1.2.3.4:11010",
            ConnFamilyPref::PreferIpv6
        ));
    }

    #[test]
    fn unparsable_listener_url_is_allowed() {
        assert!(listener_matches_prefer_family(
            "not a url at all",
            ConnFamilyPref::PreferIpv4
        ));
        assert!(listener_matches_prefer_family(
            "not a url at all",
            ConnFamilyPref::PreferIpv6
        ));
    }

    #[test]
    fn filter_listeners_auto_returns_input_unchanged() {
        let listeners = vec![
            "udp://0.0.0.0:11010".to_string(),
            "udp://[::]:11010".to_string(),
            "tcp://example.com:11010".to_string(),
        ];
        let filtered =
            filter_listeners_by_prefer_family(listeners.clone(), ConnFamilyPref::Auto, |l| {
                l.as_str()
            });
        assert_eq!(filtered, listeners);
    }

    #[test]
    fn filter_listeners_drops_other_family_and_keeps_order() {
        let listeners = vec![
            "udp://0.0.0.0:11010".to_string(),
            "udp://[::]:11010".to_string(),
            "tcp://example.com:11010".to_string(),
            "tcp://1.2.3.4:11010".to_string(),
        ];
        let filtered =
            filter_listeners_by_prefer_family(listeners.clone(), ConnFamilyPref::PreferIpv4, |l| {
                l.as_str()
            });
        assert_eq!(
            filtered,
            vec![
                "udp://0.0.0.0:11010".to_string(),
                "tcp://example.com:11010".to_string(),
                "tcp://1.2.3.4:11010".to_string(),
            ]
        );

        let filtered =
            filter_listeners_by_prefer_family(listeners.clone(), ConnFamilyPref::PreferIpv6, |l| {
                l.as_str()
            });
        assert_eq!(
            filtered,
            vec![
                "udp://[::]:11010".to_string(),
                "tcp://example.com:11010".to_string(),
            ]
        );
    }

    #[test]
    fn filter_listeners_works_on_proto_url() {
        let listeners = vec![
            easytier_proto::common::Url {
                url: "udp://0.0.0.0:11010".into(),
            },
            easytier_proto::common::Url {
                url: "udp://[::]:11010".into(),
            },
        ];
        let filtered =
            filter_listeners_by_prefer_family(listeners, ConnFamilyPref::PreferIpv4, |listener| {
                listener.url.as_str()
            });
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].url, "udp://0.0.0.0:11010");
    }

    #[test]
    fn prefer_ip_version_maps_preference() {
        assert_eq!(prefer_ip_version(ConnFamilyPref::Auto), IpVersion::Both);
        assert_eq!(prefer_ip_version(ConnFamilyPref::PreferIpv4), IpVersion::V4);
        assert_eq!(prefer_ip_version(ConnFamilyPref::PreferIpv6), IpVersion::V6);
    }

    #[test]
    fn auto_keeps_upstream_direct_conn_semantics() {
        let family_predicate_called = Cell::new(false);
        let direct_predicate_called = Cell::new(false);
        let satisfied = peer_satisfied_by(
            ConnFamilyPref::Auto,
            |_| {
                family_predicate_called.set(true);
                false
            },
            || {
                direct_predicate_called.set(true);
                true
            },
        );
        assert!(satisfied);
        assert!(direct_predicate_called.get());
        assert!(!family_predicate_called.get());
    }

    #[test]
    fn prefer_ipv4_keeps_trying_while_only_ipv6_connected() {
        // 已经连上 IPv6 时，prefer_ipv4 仍判定「未满足」→ 继续尝试 IPv4。
        let satisfied = peer_satisfied_by(
            ConnFamilyPref::PreferIpv4,
            |family| family == ConnFamily::Ipv6, // 只连上了 IPv6
            || true,
        );
        assert!(!satisfied);
    }

    #[test]
    fn prefer_ipv4_stops_once_ipv4_connected() {
        let satisfied = peer_satisfied_by(
            ConnFamilyPref::PreferIpv4,
            |family| family == ConnFamily::Ipv4,
            || false,
        );
        assert!(satisfied);
    }

    #[test]
    fn prefer_ipv4_never_uses_direct_conn_predicate() {
        // prefer 模式只能用 has_conn_of_family（含打洞连接）；
        // 一旦用 has_directly_connected_conn（排除打洞连接），
        // 打洞打通后判定恒为 false → 无限重试 + listener 被 300s 拉黑。
        let direct_predicate_called = Cell::new(false);
        let satisfied = peer_satisfied_by(
            ConnFamilyPref::PreferIpv4,
            |_| true,
            || {
                direct_predicate_called.set(true);
                false
            },
        );
        assert!(satisfied);
        assert!(!direct_predicate_called.get());
    }

    #[test]
    fn prefer_ipv6_keeps_trying_while_only_ipv4_connected() {
        let satisfied = peer_satisfied_by(
            ConnFamilyPref::PreferIpv6,
            |family| family == ConnFamily::Ipv4, // 只连上了 IPv4
            || true,
        );
        assert!(!satisfied);
    }

    #[test]
    fn prefer_ipv6_stops_once_ipv6_connected() {
        let satisfied = peer_satisfied_by(
            ConnFamilyPref::PreferIpv6,
            |family| family == ConnFamily::Ipv6,
            || false,
        );
        assert!(satisfied);
    }
}
