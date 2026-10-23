//! 通用限流工具：令牌桶（单连接发送速率）与失败窗口（配对码暴力尝试防护）。
//!
//! 令牌桶采用「背压优先」策略：正常情况直接取令牌；不足时等待补令牌（让读循环暂停，
//! 通过 TCP 窗口自然给发送端施加背压，不丢帧）；只有等待超过上限才拒绝该帧。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 令牌桶：每秒补充 `rate_per_sec` 个令牌，最多积压 `burst` 个。
///
/// `rate_per_sec == 0` 时表示关闭限速（`new` 返回 `None`）。
pub(crate) struct TokenBucket {
    tokens: f64,
    last: Instant,
    rate: f64,
    burst: f64,
}

impl TokenBucket {
    pub(crate) fn new(rate_per_sec: u32, burst: u32) -> Option<Self> {
        if rate_per_sec == 0 {
            return None;
        }
        let burst = burst.max(1) as f64;
        Some(Self {
            tokens: burst,
            last: Instant::now(),
            rate: rate_per_sec as f64,
            burst,
        })
    }

    /// 尝试立即消耗 1 个令牌（不等待）。
    pub(crate) fn try_acquire(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// 背压式获取 1 个令牌：不足时按补充速率等待，最多等待 `max_wait`。
    ///
    /// - 返回 `true`：已拿到令牌，调用方应继续处理该帧；
    /// - 返回 `false`：等待超时仍未补足（调用方按最后防线丢弃该帧并回 `rate_limited`）。
    ///
    /// 该函数只影响当前连接的任务（每连接独立令牌桶），等待期间其他连接不受影响。
    pub(crate) async fn acquire(&mut self, max_wait: Duration) -> bool {
        if self.try_acquire() {
            return true;
        }
        let deadline = Instant::now() + max_wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            self.refill();
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return true;
            }
            // 补足 1 个令牌还差多少时间；下限 1ms 避免忙等，上限为剩余等待时间。
            let seconds_needed = (1.0 - self.tokens) / self.rate;
            let wait = Duration::from_secs_f64(seconds_needed.max(0.001)).min(remaining);
            tokio::time::sleep(wait).await;
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        self.tokens = (self.tokens + elapsed.as_secs_f64() * self.rate).min(self.burst);
    }
}

/// 每设备配对加入失败窗口（滑动 60s）。
///
/// 协议 §4.3：每设备每分钟最多 5 次 join 尝试，失败即销毁登记。
pub(crate) struct JoinLimiter {
    limit: u32,
    window: Duration,
    failures: std::sync::Mutex<std::collections::HashMap<String, VecDeque<Instant>>>,
}

impl JoinLimiter {
    pub(crate) fn new(limit_per_min: u32) -> Self {
        Self {
            limit: limit_per_min,
            window: Duration::from_secs(60),
            failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 该设备当前是否已被限流（不记录本次尝试）。
    pub(crate) fn is_limited(&self, device_id: &str) -> bool {
        if self.limit == 0 {
            return false;
        }
        let mut guard = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let events = guard.entry(device_id.to_string()).or_default();
        Self::prune_queue(events, now, self.window);
        events.len() >= self.limit as usize
    }

    /// 记录一次失败尝试。
    pub(crate) fn record_failure(&self, device_id: &str) {
        if self.limit == 0 {
            return;
        }
        let mut guard = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let events = guard.entry(device_id.to_string()).or_default();
        Self::prune_queue(events, now, self.window);
        events.push_back(now);
    }

    /// 清理空的/过期的设备条目（后台清理任务调用，避免长期运行内存增长）。
    pub(crate) fn prune(&self) {
        let mut guard = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        guard.retain(|_, events| {
            Self::prune_queue(events, now, self.window);
            !events.is_empty()
        });
    }

    fn prune_queue(events: &mut VecDeque<Instant>, now: Instant, window: Duration) {
        while let Some(front) = events.front() {
            if now.saturating_duration_since(*front) >= window {
                events.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_respects_burst() {
        let mut bucket = TokenBucket::new(1, 2).expect("限速应启用");
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire(), "burst 用尽后应被拒绝");
    }

    #[test]
    fn token_bucket_can_be_disabled() {
        assert!(TokenBucket::new(0, 100).is_none());
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(50, 1).expect("限速应启用");
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());
        std::thread::sleep(Duration::from_millis(80));
        assert!(bucket.try_acquire(), "80ms 后应补充至少 1 个令牌");
    }

    #[tokio::test]
    async fn acquire_waits_for_refill_within_max_wait() {
        // 100 帧/s → 补 1 个令牌约 10ms；等待上限 500ms 应成功拿到。
        let mut bucket = TokenBucket::new(100, 1).expect("限速应启用");
        assert!(bucket.acquire(Duration::from_millis(500)).await);
        let start = Instant::now();
        assert!(bucket.acquire(Duration::from_millis(500)).await);
        assert!(
            start.elapsed() >= Duration::from_millis(5),
            "第二个令牌应经过背压等待"
        );
    }

    #[tokio::test]
    async fn acquire_times_out_when_max_wait_too_short() {
        // 1 帧/s → 补 1 个令牌需 1s，等待上限 5ms 必然超时。
        let mut bucket = TokenBucket::new(1, 1).expect("限速应启用");
        assert!(bucket.acquire(Duration::from_millis(5)).await);
        assert!(
            !bucket.acquire(Duration::from_millis(5)).await,
            "等待超时后应返回 false（由调用方丢弃该帧并回 rate_limited）"
        );
    }

    #[test]
    fn join_limiter_blocks_after_limit() {
        let limiter = JoinLimiter::new(2);
        assert!(!limiter.is_limited("dev-a"));
        limiter.record_failure("dev-a");
        assert!(!limiter.is_limited("dev-a"));
        limiter.record_failure("dev-a");
        assert!(limiter.is_limited("dev-a"));
        assert!(!limiter.is_limited("dev-b"), "限流按设备隔离");
    }

    #[test]
    fn join_limiter_can_be_disabled() {
        let limiter = JoinLimiter::new(0);
        for _ in 0..10 {
            limiter.record_failure("dev-a");
        }
        assert!(!limiter.is_limited("dev-a"));
    }

    #[test]
    fn join_limiter_prune_removes_empty_entries() {
        let limiter = JoinLimiter::new(3);
        limiter.record_failure("dev-a");
        limiter.prune();
        assert!(!limiter.is_limited("dev-a"));
    }
}
