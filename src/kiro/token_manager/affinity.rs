//! 有界的会话成功绑定。按最近成功时间清理，读请求不延长 TTL。

use super::*;
use std::collections::BTreeSet;

pub(crate) type AffinityKey = [u8; 32];

/// 只接受标准 UUID，规范化后哈希；不保存原始会话标识或请求正文。
pub(crate) fn session_key(session: &str) -> Option<AffinityKey> {
    if session.len() != 36 {
        return None;
    }
    let uuid = uuid::Uuid::parse_str(session).ok()?;
    Some(Sha256::digest(uuid.as_bytes()).into())
}

#[derive(Default)]
pub(super) struct SessionAffinity {
    bindings: HashMap<AffinityKey, (u64, Instant)>,
    oldest: BTreeSet<(Instant, AffinityKey)>,
}

impl SessionAffinity {
    fn prune(&mut self, now: Instant, ttl: StdDuration) {
        while let Some(&(updated, key)) = self.oldest.first() {
            if now.saturating_duration_since(updated) < ttl {
                break;
            }
            self.oldest.remove(&(updated, key));
            self.bindings.remove(&key);
        }
    }

    pub(super) fn preferred(
        &mut self,
        key: &AffinityKey,
        now: Instant,
        ttl: StdDuration,
    ) -> Option<u64> {
        self.prune(now, ttl);
        self.bindings.get(key).map(|(id, _)| *id)
    }

    /// 返回是否从仍有效的旧账号切换到了新账号。
    fn succeeded(
        &mut self,
        key: AffinityKey,
        id: u64,
        now: Instant,
        ttl: StdDuration,
        maximum: usize,
    ) -> bool {
        self.prune(now, ttl);
        let previous = self.bindings.remove(&key);
        if let Some((_, updated)) = previous {
            self.oldest.remove(&(updated, key));
        }
        while self.bindings.len() >= maximum {
            let Some((updated, oldest_key)) = self.oldest.pop_first() else {
                break;
            };
            let removed = self.bindings.remove(&oldest_key);
            debug_assert_eq!(removed.map(|(_, at)| at), Some(updated));
        }
        self.bindings.insert(key, (id, now));
        self.oldest.insert((now, key));
        previous.is_some_and(|(old_id, _)| old_id != id)
    }
}

impl MultiTokenManager {
    /// 在 Messages 响应完整且通过业务校验后调用，取消/空响应不更新绑定。
    pub(crate) fn report_session_success(&self, key: Option<AffinityKey>, id: u64) {
        let settings = &self.config.session_affinity;
        let Some(key) = key.filter(|_| settings.enabled) else {
            return;
        };
        // 与选择时保持 entries -> session_affinity 的锁顺序，删除后的迟到响应不建绑定。
        let entries = self.entries.lock();
        if !entries
            .iter()
            .any(|entry| entry.id == id && !entry.disabled)
        {
            return;
        }
        let switched = self.session_affinity.lock().succeeded(
            key,
            id,
            Instant::now(),
            StdDuration::from_secs(settings.ttl_seconds),
            settings.max_entries,
        );
        if switched {
            self.concurrency_metrics
                .affinity_switch_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_and_capacity_follow_successes_without_growing_the_time_index() {
        let mut cache = SessionAffinity::default();
        let now = Instant::now();
        let ttl = StdDuration::from_secs(10);
        let a = [1; 32];
        let b = [2; 32];
        let c = [3; 32];
        cache.succeeded(a, 1, now, ttl, 2);
        cache.succeeded(b, 2, now + StdDuration::from_secs(1), ttl, 2);
        assert!(cache.succeeded(a, 3, now + StdDuration::from_secs(2), ttl, 2));
        cache.succeeded(c, 4, now + StdDuration::from_secs(3), ttl, 2);
        assert_eq!(
            cache.preferred(&b, now + StdDuration::from_secs(3), ttl),
            None
        );
        assert_eq!(
            cache.preferred(&a, now + StdDuration::from_secs(11), ttl),
            Some(3)
        );
        // A read just before expiry does not extend the successful binding.
        assert_eq!(
            cache.preferred(&a, now + StdDuration::from_secs(12), ttl),
            None
        );
        assert_eq!(
            cache.preferred(&c, now + StdDuration::from_secs(13), ttl),
            None
        );
        assert!(cache.bindings.is_empty());
        assert!(cache.oldest.is_empty());
        for n in 0..100 {
            cache.succeeded(a, 1, now + StdDuration::from_secs(n), ttl, 2);
        }
        assert_eq!(cache.bindings.len(), 1);
        assert_eq!(cache.oldest.len(), 1);
    }

    #[test]
    fn session_keys_normalize_uuids_and_reject_invalid_identifiers() {
        let uuid = "abcdef01-2345-6789-abcd-0123456789ab";
        assert!(session_key(uuid).is_some());
        assert_eq!(session_key(uuid), session_key(&uuid.to_uppercase()));
        for invalid in ["", "not-a-session", "abcdef0123456789abcd0123456789ab"] {
            assert_eq!(session_key(invalid), None);
        }
    }
}
