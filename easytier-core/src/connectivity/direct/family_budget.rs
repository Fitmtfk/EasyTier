//! 偏好族直连的重试预算与冷却（spec §4.6，brief Task 6）。
//!
//! `auto` 模式完全不受影响（预算各项操作均无副作用，上游无限退避循环原样保留）。
//! 非 `auto` 偏好下：
//! - 一个「轮（round）」= `try_direct_connect` 外层退避循环的一次迭代
//!   （拉一次 IP 列表 + 跑完第一轮/兜底轮 listener），**不是**一次 URL/一次 listen。
//! - 每个窗口（window）最多跑 [`FAMILY_CONNECT_ROUNDS_PER_WINDOW`] 轮；
//!   跑满仍未满足偏好族 → 进入 [`FAMILY_CONNECT_WINDOW_COOLDOWN`] 冷却。
//! - 冷却结束后进入下一个窗口（轮数清零，窗口数累计）；
//!   累计 [`FAMILY_CONNECT_MAX_WINDOWS`] 个窗口无效即「耗尽」。
//! - 耗尽后只有在「偏好设置变化」或「该 peer 连接集合变化」时才重置预算
//!   （放弃时刻的 `prefer_snapshot` / `conn_snapshot`，见 [`FamilyBudgetState`]）。
//!
//! 一切成功判定以 spec §10.3 的 R-h 为准：只有「偏好族已满足」才记成功并清零预算；
//! 兜底（AnyFamily）连通**不算**成功，预算继续推进。

use std::collections::HashMap;

use crate::{
    config::PeerId,
    foundation::time::{Duration, Instant},
    peers::conn::family::ConnFamilyPref,
};

/// 每个窗口内允许的直连轮数。
pub(crate) const FAMILY_CONNECT_ROUNDS_PER_WINDOW: u32 = 3;

/// 窗口跑满后的冷却时长。
pub(crate) const FAMILY_CONNECT_WINDOW_COOLDOWN: Duration = Duration::from_secs(120);

/// 预算耗尽（不再重试，直到恢复条件触发）前允许的窗口数。
pub(crate) const FAMILY_CONNECT_MAX_WINDOWS: u32 = 5;

/// 单个 peer 的偏好族直连预算状态。
///
/// 记录「从最近一次成功/重置以来」的窗口数、窗口内已耗轮数与冷却截止时刻。
///冷却开始前已完成轮数仍计入 `rounds`，直到冷却结束时才清零进入下一窗口。
pub(crate) struct FamilyBudgetState {
    /// 已耗掉的窗口数（跨冷却累计）。
    pub(crate) windows: u32,
    /// 当前窗口内已完成的轮数。
    pub(crate) rounds: u32,
    /// 冷却截止时刻（冷却结束后清 `None`，`rounds` 同步清零）。
    pub(crate) cooldown_until: Option<Instant>,
    /// 预算是否已耗尽（进入后不再重试，直到恢复条件触发）。
    pub(crate) exhausted: bool,
    /// 耗尽时刻记录的 peer 连接数快照（恢复判定用，见 [`family_attempt_allowed`]）。
    pub(crate) conn_snapshot: Option<usize>,
    /// 耗尽时刻记录的偏好设置快照（恢复判定用）。
    pub(crate) prefer_snapshot: Option<ConnFamilyPref>,
}

impl Default for FamilyBudgetState {
    fn default() -> Self {
        Self {
            windows: 0,
            rounds: 0,
            cooldown_until: None,
            exhausted: false,
            conn_snapshot: None,
            prefer_snapshot: None,
        }
    }
}

impl FamilyBudgetState {
    /// 是否还允许再跑一轮直连。
    ///
    /// 满足下列全部条件才返回 `true`：
    /// - 预算未耗尽；
    /// - 不在冷却中，或冷却刚好在本次调用时到期（到期即开新窗口）。
    ///
    /// 注意：本函数**会**推进状态（冷却到期时开新窗口），因此使用方必须在
    /// 打算真的开跑时才调用。
    pub(crate) fn allow_round(&mut self, now: Instant) -> bool {
        if self.exhausted {
            return false;
        }
        if let Some(deadline) = self.cooldown_until {
            if now < deadline {
                return false;
            }
            // 冷却到期：开新窗口 —— 轮数清零（窗口累计），冷却清空。
            self.cooldown_until = None;
            self.rounds = 0;
        }
        true
    }

    /// 记录一次轮失败（跑完外层退避循环的一次迭代仍未满足偏好族）。
    ///
    /// 已耗尽的预算不再推进（保持终态，直到恢复条件触发）。
    pub(crate) fn record_round_failure(&mut self, now: Instant) {
        if self.exhausted {
            return;
        }
        self.rounds += 1;
        if self.rounds >= FAMILY_CONNECT_ROUNDS_PER_WINDOW {
            self.windows += 1;
            if self.windows >= FAMILY_CONNECT_MAX_WINDOWS {
                self.exhausted = true;
            } else {
                self.rounds = 0;
                self.cooldown_until = Some(now + FAMILY_CONNECT_WINDOW_COOLDOWN);
            }
        }
    }

    /// 记录一次成功（偏好族已满足）。清零全部预算状态（含快照）。
    pub(crate) fn record_success(&mut self) {
        *self = Self::default();
    }
}

/// 全体 peer 的预算表。
pub(crate) type FamilyBudgetMap = HashMap<PeerId, FamilyBudgetState>;

/// 该 peer 是否允许再跑一轮直连（budget 判定的总入口）。
///
/// - `auto`（R-f）：预算不生效，恒允许 —— 上游无限退避循环原样保留。
/// - 已耗尽的预算（R-i）：偏好设置或连接集合相对「放弃时的快照」发生变化时，
///   重置预算后重新判定。
///
/// 未有两「放弃快照」之一变化时，按现有状态判定
/// （`allow_round` 的窗口/冷却/耗尽约束全部生效）。
pub(crate) fn family_attempt_allowed(
    map: &mut FamilyBudgetMap,
    dst_peer_id: PeerId,
    prefer: ConnFamilyPref,
    now: Instant,
    conn_total: usize,
) -> bool {
    if prefer == ConnFamilyPref::Auto {
        return true;
    }
    let Some(state) = map.get_mut(&dst_peer_id) else {
        return true;
    };
    if state.exhausted
        && (state.prefer_snapshot != Some(prefer) || state.conn_snapshot != Some(conn_total))
    {
        *state = FamilyBudgetState::default();
    }
    state.allow_round(now)
}

/// 记录一次轮失败（跑完一轮仍不满足偏好族，R-d 的唯一记账点）。
///
/// `auto` 不记账（R-f）。预算耗尽的瞬间记下偏好与连接数快照（R-i）。
pub(crate) fn family_round_failure(
    map: &mut FamilyBudgetMap,
    dst_peer_id: PeerId,
    prefer: ConnFamilyPref,
    now: Instant,
    conn_total: usize,
) {
    if prefer == ConnFamilyPref::Auto {
        return;
    }
    let state = map.entry(dst_peer_id).or_default();
    state.record_round_failure(now);
    if state.exhausted {
        // R-i：耗尽时记下放弃时刻的偏好与连接集合快照。
        state.conn_snapshot = Some(conn_total);
        state.prefer_snapshot = Some(prefer);
    }
}

/// 记录一次成功（偏好族已满足，R-h 的唯一成功口径）。
///
/// `auto` 不记账（R-f，本来就无预算可清）。非 auto 下清零该 peer 的预算。
pub(crate) fn family_attempt_succeeded(
    map: &mut FamilyBudgetMap,
    dst_peer_id: PeerId,
    prefer: ConnFamilyPref,
) {
    if prefer == ConnFamilyPref::Auto {
        return;
    }
    let Some(state) = map.get_mut(&dst_peer_id) else {
        return;
    };
    state.record_success();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// 三轮后窗口不再放行（第 4 次请求被拒）。
    #[test]
    fn window_stops_after_three_rounds() {
        let t = t0();
        let mut state = FamilyBudgetState::default();
        assert!(state.allow_round(t));
        state.record_round_failure(t);
        assert!(state.allow_round(t));
        state.record_round_failure(t);
        assert!(state.allow_round(t));
        state.record_round_failure(t);
        // 第 3 次失败进入冷却：同窗口第 4 轮不放行。
        assert!(state.cooldown_until.is_some());
        assert!(!state.allow_round(t));
    }

    /// 冷却结束后放行，且轮数清零、窗口数保留。
    #[test]
    fn cooldown_expires_after_window() {
        let t = t0();
        let mut state = FamilyBudgetState::default();
        for _ in 0..3 {
            assert!(state.allow_round(t));
            state.record_round_failure(t);
        }
        assert!(!state.allow_round(t));

        // 冷却期内不放行；到期后放行且进入下一窗口。
        let after_cooldown = t + FAMILY_CONNECT_WINDOW_COOLDOWN + Duration::from_secs(1);
        assert!(state.allow_round(after_cooldown));
        assert_eq!(state.rounds, 0);
        assert_eq!(state.windows, 1);
        assert!(state.cooldown_until.is_none());
    }

    /// 5 个窗口跑满后预算耗尽，永久不放行。
    #[test]
    fn stops_after_five_windows() {
        let t = t0();
        let mut state = FamilyBudgetState::default();
        for window in 0..FAMILY_CONNECT_MAX_WINDOWS {
            for _ in 0..3 {
                assert!(state.allow_round(t));
                state.record_round_failure(t);
            }
            assert!(!state.allow_round(t), "window {window} closed early");
            // 冷却结束进入下一窗口（最后一个窗口耗尽，后面整段不再放行）。
            let next_window = t + FAMILY_CONNECT_WINDOW_COOLDOWN + Duration::from_secs(1);
            if window + 1 < FAMILY_CONNECT_MAX_WINDOWS {
                assert!(state.allow_round(next_window));
            }
        }
        assert!(state.exhausted);
        // 耗尽后无论何时都不放行。
        let far = t + FAMILY_CONNECT_WINDOW_COOLDOWN * 100;
        assert!(!state.allow_round(far));
    }

    /// 成功后预算全部清零（窗口、轮、冷却、快照）。
    #[test]
    fn success_clears_budget() {
        let t = t0();
        let mut state = FamilyBudgetState::default();
        for _ in 0..3 {
            state.allow_round(t);
            state.record_round_failure(t);
        }
        // 已进冷却。成功清零后立刻可比新预算更宽松。
        state.record_success();
        assert_eq!(state.rounds, 0);
        assert_eq!(state.windows, 0);
        assert!(state.cooldown_until.is_none());
        assert!(!state.exhausted);
        assert!(state.conn_snapshot.is_none());
        assert!(state.prefer_snapshot.is_none());
        assert!(state.allow_round(t));
    }

    /// R-9：耗尽后偏好设置变化 → 重置预算。
    #[test]
    fn exhausted_budget_resets_when_preference_changes() {
        let t = t0();
        let mut map = FamilyBudgetMap::default();
        let peer: PeerId = 1;
        // 5 个窗口耗尽。
        let mut now = t;
        for _ in 0..FAMILY_CONNECT_MAX_WINDOWS {
            for _ in 0..FAMILY_CONNECT_ROUNDS_PER_WINDOW {
                assert!(family_attempt_allowed(
                    &mut map,
                    peer,
                    ConnFamilyPref::PreferIpv4,
                    now,
                    0
                ));
                family_round_failure(&mut map, peer, ConnFamilyPref::PreferIpv4, now, 0);
                now += Duration::from_secs(1);
            }
            now += FAMILY_CONNECT_WINDOW_COOLDOWN;
        }
        assert!(
            map.get(&peer).is_some_and(|s| s.exhausted),
            "should be exhausted"
        );
        // 同偏好、同连接数 → 仍不放行。
        let next = now + Duration::from_secs(1);
        assert!(!family_attempt_allowed(
            &mut map,
            peer,
            ConnFamilyPref::PreferIpv4,
            next,
            0
        ));
        // 偏好改成另一非 auto 值 → 重置 → 放行。
        assert!(family_attempt_allowed(
            &mut map,
            peer,
            ConnFamilyPref::PreferIpv6,
            next,
            0
        ));
        // 重置后回到 0 窗口、0 轮的新状态。
        let state = map.get(&peer).unwrap();
        assert_eq!(state.windows, 0);
        assert_eq!(state.rounds, 0);
    }

    /// R-9：耗尽后连接集合变化 → 重置预算。
    #[test]
    fn exhausted_budget_resets_when_conn_count_changes() {
        let t = t0();
        let mut map = FamilyBudgetMap::default();
        let peer: PeerId = 2;
        let mut now = t;
        for _ in 0..FAMILY_CONNECT_MAX_WINDOWS {
            for _ in 0..FAMILY_CONNECT_ROUNDS_PER_WINDOW {
                assert!(family_attempt_allowed(
                    &mut map,
                    peer,
                    ConnFamilyPref::PreferIpv4,
                    now,
                    0
                ));
                family_round_failure(&mut map, peer, ConnFamilyPref::PreferIpv4, now, 0);
                now += Duration::from_secs(1);
            }
            now += FAMILY_CONNECT_WINDOW_COOLDOWN;
        }
        assert!(
            map.get(&peer).is_some_and(|s| s.exhausted),
            "should be exhausted"
        );
        let next = now + Duration::from_secs(1);
        // 同偏好、同连接数 → 仍不放行。
        assert!(!family_attempt_allowed(
            &mut map,
            peer,
            ConnFamilyPref::PreferIpv4,
            next,
            0
        ));
        // 连接数变了（例如新来了一条连接）→ 重置 → 放行。
        assert!(family_attempt_allowed(
            &mut map,
            peer,
            ConnFamilyPref::PreferIpv4,
            next,
            1
        ));
    }

    /// R-9：兜底建立非偏好族连接**不算**成功 —— 预算继续按窗口推进。
    /// 若误把兜底当成功（清零轮数），第 3 轮失败将处于新窗口第 1 轮、仍放行；
    /// 正确行为下第 3 轮失败直接关窗进冷却。
    #[test]
    fn fallback_conn_does_not_clear_budget() {
        let t = t0();
        let mut map = FamilyBudgetMap::default();
        let peer: PeerId = 3;
        // 前两轮失败：预算已记 2 轮。
        family_round_failure(&mut map, peer, ConnFamilyPref::PreferIpv4, t, 0);
        family_round_failure(&mut map, peer, ConnFamilyPref::PreferIpv4, t, 0);
        assert_eq!(map.get(&peer).unwrap().rounds, 2);

        // 兜底建立了非偏好族连接（conn_total 由 0→1）：生产代码不会因此调
        // family_attempt_succeeded（R-h）；这里模拟整个时间线 —— 第 3 轮在前两轮
        // 仍被计数的状态下发生，最终直接关窗。
        assert!(family_attempt_allowed(
            &mut map,
            peer,
            ConnFamilyPref::PreferIpv4,
            t,
            1
        ));
        family_round_failure(&mut map, peer, ConnFamilyPref::PreferIpv4, t, 1);
        let state = map.get(&peer).unwrap();
        assert_eq!(state.windows, 1, "3rd failure still closes the window");
        assert_eq!(state.rounds, 0, "cooldown resets rounds, windows retained");
        assert!(state.cooldown_until.is_some());
        // 关窗后不放行（若兜底被误当成功，这里 rounds 应为 1、仍放行）。
        assert!(!family_attempt_allowed(
            &mut map,
            peer,
            ConnFamilyPref::PreferIpv4,
            t,
            1
        ));
    }
}
