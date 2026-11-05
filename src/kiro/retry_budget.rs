//! A client request shares one attempt budget through handler and body polling.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::time::Instant;

tokio::task_local! { pub(crate) static CURRENT: Arc<RetryBudget>; }

pub(crate) struct RetryBudget {
    remaining: AtomicUsize,
    deadline: Instant,
}

#[derive(Debug)]
pub(crate) struct RetryBudgetExceeded;

impl std::fmt::Display for RetryBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Request upstream attempt budget or total deadline exceeded")
    }
}
impl std::error::Error for RetryBudgetExceeded {}

impl RetryBudget {
    pub(crate) fn new(attempts: usize, deadline: Instant) -> Arc<Self> {
        Arc::new(Self {
            remaining: AtomicUsize::new(attempts),
            deadline,
        })
    }

    pub(crate) fn consume(&self) -> Result<(), RetryBudgetExceeded> {
        if Instant::now() >= self.deadline {
            return Err(RetryBudgetExceeded);
        }
        self.remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| RetryBudgetExceeded)
    }
}

pub(crate) fn consume() -> Result<(), RetryBudgetExceeded> {
    CURRENT
        .try_with(|budget| budget.consume())
        .unwrap_or(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_attempts_cannot_overrun_budget_and_deadline_is_terminal() {
        let budget = RetryBudget::new(3, Instant::now() + std::time::Duration::from_secs(60));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let budget = budget.clone();
            tasks.push(tokio::spawn(async move { budget.consume().is_ok() }));
        }
        let mut admitted = 0;
        for task in tasks {
            admitted += usize::from(task.await.unwrap());
        }
        assert_eq!(admitted, 3);
        assert!(RetryBudget::new(9, Instant::now()).consume().is_err());
    }
}
