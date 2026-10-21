use easytier_proto::common::TunnelInfo;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_pref_falls_back_to_auto() {
        assert_eq!(
            ConnFamilyPref::from_flag_str("prefer_ipv4"),
            ConnFamilyPref::PreferIpv4
        );
        assert_eq!(
            ConnFamilyPref::from_flag_str(" prefer_ipv6 "),
            ConnFamilyPref::PreferIpv6
        );
        assert_eq!(ConnFamilyPref::from_flag_str("ipv4"), ConnFamilyPref::Auto);
        assert_eq!(ConnFamilyPref::from_flag_str(""), ConnFamilyPref::Auto);
        // 大小写不符同样回落 Auto，不 panic。
        assert_eq!(
            ConnFamilyPref::from_flag_str("PREFER_IPV4"),
            ConnFamilyPref::Auto
        );
        assert_eq!(
            ConnFamilyPref::from_flag_str("Prefer_Ipv6"),
            ConnFamilyPref::Auto
        );
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
            remote_addr: Some(easytier_proto::common::Url {
                url: "tcp://10.0.0.1:11010".into(),
            }),
            ..Default::default()
        };
        assert_eq!(tunnel_info_family(Some(&v4)), ConnFamily::Ipv4);
        assert_eq!(tunnel_info_family(None), ConnFamily::Unknown);
    }
}
