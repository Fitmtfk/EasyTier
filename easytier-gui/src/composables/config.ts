/**
 * 配置持久化相关的函数
 * 用于保存和加载应用程序的各种配置状态
 */

/**
 * 保存上次使用的网络实例 ID
 * @param instanceId 网络实例 ID
 */
export function saveLastNetworkInstanceId(instanceId: string) {
    localStorage.setItem('last_network_instance_id', instanceId)
}

/**
 * 加载上次使用的网络实例 ID
 * @returns 上次使用的网络实例 ID，如果没有则返回 null
 */
export function loadLastNetworkInstanceId(): string | null {
    return localStorage.getItem('last_network_instance_id')
}

export function saveVpnTileStoppedInstanceIds(instanceIds: string[]) {
    localStorage.setItem('vpn_tile_stopped_instance_ids', JSON.stringify(instanceIds))
}

export function loadVpnTileStoppedInstanceIds(): string[] {
    try {
        const value: unknown = JSON.parse(localStorage.getItem('vpn_tile_stopped_instance_ids') || '[]')
        return Array.isArray(value) ? value.filter((id): id is string => typeof id === 'string') : []
    }
    catch {
        return []
    }
}

const CUSTOM_VPN_DNS_KEY = 'custom_vpn_dns'

/**
 * 校验字符串是否为合法 IPv4 地址（点分四段，每段 0-255，不允许前导零）
 * @param value 待校验字符串
 */
export function isValidIpv4(value: string): boolean {
    const parts = value.split('.')
    if (parts.length !== 4) {
        return false
    }
    return parts.every(part => /^(?:0|[1-9]\d{0,2})$/.test(part) && Number(part) <= 255)
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
    const value = localStorage.getItem(CUSTOM_VPN_DNS_KEY)
    if (value === null) {
        return null
    }
    const trimmed = value.trim()
    return trimmed.length ? trimmed : null
}
