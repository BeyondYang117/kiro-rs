//! 有界、模型感知的公平等待队列和进程内调度指标。

use super::*;
use tokio::sync::SemaphorePermit;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConcurrencySnapshot {
    pub adaptive_concurrency: bool,
    pub recovering_credentials: usize,
    pub max_in_flight: usize,
    pub max_in_flight_per_credential: u64,
    pub max_waiting: usize,
    pub wait_timeout_seconds: u64,
    pub in_flight: usize,
    pub waiting: usize,
    pub cooling_credentials: usize,
    pub saturated_credentials: usize,
    pub oldest_wait_ms: u64,
    pub admitted_total: u64,
    pub queued_total: u64,
    pub queue_full_total: u64,
    pub wait_timeout_total: u64,
    pub cancelled_wait_total: u64,
    pub rate_limited_total: u64,
    pub adaptive_reduction_total: u64,
    /// 所有已离队调用（含取消和错误）的耗时汇总；不含仍在等待的调用。
    pub finished_wait_total: u64,
    pub total_wait_ms: u64,
    pub max_wait_ms: u64,
}

/// 账号级 AIMD：429 后减半、冷却后单请求探测，每个成功窗口增加一个名额。
pub(super) struct AdaptiveLimit {
    limit: u64,
    probing: bool,
    successes: u64,
    pub(super) epoch: Arc<()>,
}

impl AdaptiveLimit {
    pub(super) fn new(limit: u64) -> Self {
        Self {
            limit,
            probing: false,
            successes: 0,
            epoch: Arc::new(()),
        }
    }

    pub(super) fn rate_limited(&mut self) -> bool {
        let previous_limit = self.limit;
        self.limit = (self.limit / 2).max(1);
        self.probing = true;
        self.successes = 0;
        // 限流前已在途请求的迟到成功不能推动本轮恢复。
        self.epoch = Arc::new(());
        self.limit < previous_limit
    }

    fn effective_limit(&self) -> u64 {
        if self.probing { 1 } else { self.limit }
    }

    pub(super) fn probe_target(&self) -> Option<u64> {
        self.probing.then_some(self.limit)
    }

    fn succeeded(&mut self, maximum: u64) {
        if self.probing {
            self.probing = false;
        } else if self.limit < maximum {
            self.successes += 1;
            if self.successes >= self.limit {
                self.limit += 1;
                self.successes = 0;
            }
        }
    }
}

#[derive(Default)]
pub(super) struct ConcurrencyMetrics {
    admitted_total: AtomicU64,
    queued_total: AtomicU64,
    queue_full_total: AtomicU64,
    wait_timeout_total: AtomicU64,
    cancelled_wait_total: AtomicU64,
    pub(super) rate_limited_total: AtomicU64,
    pub(super) adaptive_reduction_total: AtomicU64,
    finished_wait_total: AtomicU64,
    total_wait_ms: AtomicU64,
    max_wait_ms: AtomicU64,
}

pub(super) struct Waiter {
    ticket: Arc<()>,
    /// 当前账号模型资格分为 Opus 和通用两类，避免每次唤醒做队列×账号的扫描。
    pub(super) requires_opus: bool,
    started: Instant,
}

/// future 被取消时也会移出队列、释放等待位并唤醒后续请求。
struct WaitingGuard<'a> {
    manager: &'a MultiTokenManager,
    ticket: Arc<()>,
    slot: Option<SemaphorePermit<'a>>,
    started: Instant,
    completed: bool,
}

impl Drop for WaitingGuard<'_> {
    fn drop(&mut self) {
        let mut queue = self.manager.wait_queue.lock();
        queue.retain(|waiter| !Arc::ptr_eq(&waiter.ticket, &self.ticket));
        self.slot.take();
        let metrics = &self.manager.concurrency_metrics;
        let wait_ms = millis(self.started.elapsed());
        metrics.total_wait_ms.fetch_add(wait_ms, Ordering::Relaxed);
        metrics.max_wait_ms.fetch_max(wait_ms, Ordering::Relaxed);
        metrics.finished_wait_total.fetch_add(1, Ordering::Relaxed);
        if !self.completed {
            metrics.cancelled_wait_total.fetch_add(1, Ordering::Relaxed);
        }
        drop(queue);
        self.manager.capacity_changed.notify_waiters();
    }
}

fn millis(duration: StdDuration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

pub(super) fn cooldown_remaining_ms(entry: &CredentialEntry) -> u64 {
    entry.cooldown.map_or(0, |(started, duration)| {
        let remaining = duration.saturating_sub(started.elapsed());
        // 向上取整，非零冷却不显示为 0ms；超大 Retry-After 饱和表示。
        millis(remaining).saturating_add(u64::from(remaining.subsec_nanos() % 1_000_000 != 0))
    })
}

impl MultiTokenManager {
    pub(super) fn effective_concurrency_limit(&self, entry: &CredentialEntry) -> u64 {
        if self.config.concurrency.adaptive_concurrency {
            entry.adaptive_limit.effective_limit()
        } else {
            self.config.concurrency.max_in_flight_per_credential
        }
    }

    /// 完整消费 2xx 响应体才恢复容量；丢弃、取消或读取错误不计成功。
    /// 与业务响应校验/成功统计分开，不把传输完成视为业务成功。
    pub(crate) fn report_transport_success(&self, context: &CallContext) {
        if !self.config.concurrency.adaptive_concurrency {
            return;
        }
        let Some(reservation) = &context.reservation else {
            return;
        };
        if reservation
            .transport_success_reported
            .swap(true, Ordering::Relaxed)
        {
            return;
        }
        let mut entries = self.entries.lock();
        let Some(entry) = entries.iter_mut().find(|entry| entry.id == context.id) else {
            return;
        };
        if entry.disabled || !Arc::ptr_eq(&entry.adaptive_limit.epoch, &reservation.adaptive_epoch)
        {
            return;
        }
        let before = self.effective_concurrency_limit(entry);
        entry
            .adaptive_limit
            .succeeded(self.config.concurrency.max_in_flight_per_credential);
        let increased = self.effective_concurrency_limit(entry) > before;
        drop(entries);
        if increased {
            self.capacity_changed.notify_waiters();
        }
    }

    pub(super) fn concurrency_snapshot(
        &self,
        queue: &[Waiter],
        entries: &[CredentialEntry],
    ) -> ConcurrencySnapshot {
        let settings = &self.config.concurrency;
        let metrics = &self.concurrency_metrics;
        ConcurrencySnapshot {
            adaptive_concurrency: settings.adaptive_concurrency,
            recovering_credentials: entries
                .iter()
                .filter(|entry| {
                    settings.adaptive_concurrency
                        && !entry.disabled
                        && (entry.adaptive_limit.probing
                            || entry.adaptive_limit.limit < settings.max_in_flight_per_credential)
                })
                .count(),
            max_in_flight: settings.max_in_flight,
            max_in_flight_per_credential: settings.max_in_flight_per_credential,
            max_waiting: settings.max_waiting,
            wait_timeout_seconds: settings.wait_timeout_seconds,
            // 被删除但响应仍在消费的账号仍占用全局名额，不能用 entries 求和。
            in_flight: settings.max_in_flight - self.in_flight_slots.available_permits(),
            waiting: queue.len(),
            cooling_credentials: entries
                .iter()
                .filter(|e| !e.disabled && cooldown_remaining_ms(e) > 0)
                .count(),
            saturated_credentials: entries
                .iter()
                .filter(|e| {
                    !e.disabled
                        && e.in_flight.load(Ordering::Relaxed)
                            >= self.effective_concurrency_limit(e)
                })
                .count(),
            oldest_wait_ms: queue
                .first()
                .map_or(0, |waiter| millis(waiter.started.elapsed())),
            admitted_total: metrics.admitted_total.load(Ordering::Relaxed),
            queued_total: metrics.queued_total.load(Ordering::Relaxed),
            queue_full_total: metrics.queue_full_total.load(Ordering::Relaxed),
            wait_timeout_total: metrics.wait_timeout_total.load(Ordering::Relaxed),
            cancelled_wait_total: metrics.cancelled_wait_total.load(Ordering::Relaxed),
            rate_limited_total: metrics.rate_limited_total.load(Ordering::Relaxed),
            adaptive_reduction_total: metrics.adaptive_reduction_total.load(Ordering::Relaxed),
            finished_wait_total: metrics.finished_wait_total.load(Ordering::Relaxed),
            total_wait_ms: metrics.total_wait_ms.load(Ordering::Relaxed),
            max_wait_ms: metrics.max_wait_ms.load(Ordering::Relaxed),
        }
    }

    pub(super) async fn reserve_credential(
        &self,
        model: Option<&str>,
    ) -> anyhow::Result<ReservedCredential> {
        let mut waiting = None;
        let result = self.reserve_credential_inner(model, &mut waiting).await;
        if let Some(guard) = waiting.as_mut() {
            guard.completed = true;
        }
        let metrics = &self.concurrency_metrics;
        match &result {
            Ok(_) => {
                metrics.admitted_total.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => match error.downcast_ref::<CapacityError>() {
                Some(CapacityError::QueueFull) => {
                    metrics.queue_full_total.fetch_add(1, Ordering::Relaxed);
                }
                Some(CapacityError::WaitTimeout) => {
                    metrics.wait_timeout_total.fetch_add(1, Ordering::Relaxed);
                }
                None => {}
            },
        }
        result
    }

    async fn reserve_credential_inner<'a>(
        &'a self,
        model: Option<&str>,
        waiting: &mut Option<WaitingGuard<'a>>,
    ) -> anyhow::Result<ReservedCredential> {
        let deadline = tokio::time::Instant::now()
            + StdDuration::from_secs(self.config.concurrency.wait_timeout_seconds);
        loop {
            // 在检查容量之前注册通知，防止释放、取消、管理操作发生在检查与等待之间。
            let notified = self.capacity_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let ready_at = {
                // 注册、顺序检查与名额预留串行完成，新请求不能越过可调度的旧请求。
                let mut queue = self.wait_queue.lock();
                let position = waiting.as_ref().map_or(queue.len(), |guard| {
                    queue
                        .iter()
                        .position(|waiter| Arc::ptr_eq(&waiter.ticket, &guard.ticket))
                        .expect("registered waiter remains in queue until dropped")
                });
                let (reserved, ready_at) =
                    self.try_reserve_credential(model, &queue[..position])?;
                if let Some(reserved) = reserved {
                    return Ok(reserved);
                }
                if waiting.is_none() {
                    let slot = self
                        .waiting_slots
                        .try_acquire()
                        .map_err(|_| CapacityError::QueueFull)?;
                    let ticket = Arc::new(());
                    let started = Instant::now();
                    queue.push(Waiter {
                        ticket: ticket.clone(),
                        requires_opus: MultiTokenManager::requires_opus(model),
                        started,
                    });
                    self.concurrency_metrics
                        .queued_total
                        .fetch_add(1, Ordering::Relaxed);
                    *waiting = Some(WaitingGuard {
                        manager: self,
                        ticket,
                        slot: Some(slot),
                        started,
                        completed: false,
                    });
                }
                ready_at
            };
            let wake_at = ready_at
                .map(tokio::time::Instant::from_std)
                .map_or(deadline, |ready| ready.min(deadline));
            tokio::select! {
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(wake_at) => {},
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CapacityError::WaitTimeout.into());
            }
        }
    }
}
