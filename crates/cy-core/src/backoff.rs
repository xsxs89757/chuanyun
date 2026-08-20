//! 重连退避。
//!
//! 指数退避 + 抖动。抖动是必需的：服务端重启时所有客户端会同时掉线，
//! 没有抖动它们就会同时回来，把刚起来的服务端再打一遍。

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    /// 抖动幅度，取值 0.0~1.0。0.3 表示实际等待在计算值的 70%~100% 之间。
    jitter: f64,
    attempt: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(60))
    }
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            jitter: 0.3,
            attempt: 0,
        }
    }

    /// 下一次该等多久。
    pub fn next_delay(&mut self) -> Duration {
        let exp = self
            .base
            .saturating_mul(2u32.saturating_pow(self.attempt.min(16)));
        let capped = exp.min(self.max);
        self.attempt = self.attempt.saturating_add(1);

        // 用等待时长本身做随机源：这里不需要密码学强度的随机数，
        // 只要各客户端之间别整齐划一就行，也就不必为此多背一个依赖。
        let noise = pseudo_random(capped.as_nanos() as u64 ^ u64::from(self.attempt));
        let factor = 1.0 - self.jitter * noise;
        capped.mul_f64(factor.clamp(0.0, 1.0))
    }

    /// 连上了就清零，下次掉线从头开始退避。
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// 已经重试了几次（连上后归零）。界面上用来显示"第 n 次重连"。
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

/// xorshift，够散就行。
fn pseudo_random(seed: u64) -> f64 {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_then_levels_off() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let delays: Vec<_> = (0..10).map(|_| b.next_delay()).collect();

        // 大致翻倍（有抖动，所以比的是区间不是精确值）
        assert!(delays[0] <= Duration::from_secs(1));
        assert!(delays[1] > delays[0]);
        assert!(delays[2] > delays[1]);
        // 最终都压在上限内
        for d in &delays {
            assert!(*d <= Duration::from_secs(60), "退避超过上限: {d:?}");
        }
        assert!(delays[9] > Duration::from_secs(30), "涨到上限附近才对");
    }

    #[test]
    fn reset_starts_over() {
        let mut b = Backoff::default();
        for _ in 0..5 {
            b.next_delay();
        }
        assert_eq!(b.attempt(), 5);
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert!(b.next_delay() <= Duration::from_secs(1));
    }

    #[test]
    fn jitter_spreads_clients_out() {
        // 同一时刻掉线的一批客户端，退避值不该整齐划一
        let delays: Vec<_> = (0..8)
            .map(|i| {
                let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
                for _ in 0..=i {
                    b.next_delay();
                }
                b.next_delay()
            })
            .collect();
        let distinct: std::collections::HashSet<_> = delays.iter().collect();
        assert!(distinct.len() > 1, "所有客户端会同时回来重连: {delays:?}");
    }

    #[test]
    fn never_overflows() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        for _ in 0..1000 {
            let d = b.next_delay();
            assert!(d <= Duration::from_secs(60));
        }
    }
}
