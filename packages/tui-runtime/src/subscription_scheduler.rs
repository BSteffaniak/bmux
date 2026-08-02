//! Runtime-owned long-lived subscription scheduling.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;

use crate::ids::SubscriptionKey;
use crate::mailbox::with_stats;
use crate::runtime::SubscriptionSender;
use crate::stats::RuntimeStats;
use crate::subscription::Subscription;

pub struct SubscriptionCompletion {
    pub key: SubscriptionKey,
    pub generation: u64,
}

struct ActiveSubscription {
    generation: u64,
    abort: tokio::task::AbortHandle,
}

pub struct SubscriptionScheduler<M> {
    tasks: JoinSet<SubscriptionCompletion>,
    active: BTreeMap<SubscriptionKey, ActiveSubscription>,
    generations: BTreeMap<SubscriptionKey, u64>,
    max_active: usize,
    sender: SubscriptionSender<M>,
    stats: Arc<Mutex<RuntimeStats>>,
}

impl<M: Send + 'static> SubscriptionScheduler<M> {
    pub fn new(
        max_active: usize,
        sender: SubscriptionSender<M>,
        stats: Arc<Mutex<RuntimeStats>>,
    ) -> Self {
        Self {
            tasks: JoinSet::new(),
            active: BTreeMap::new(),
            generations: BTreeMap::new(),
            max_active,
            sender,
            stats,
        }
    }

    pub fn replace(&mut self, subscription: Subscription<M>) {
        let key = subscription.key;
        let replacing = self.cancel(&key);
        if !replacing && self.active.len() >= self.max_active {
            with_stats(&self.stats, |stats| {
                stats.subscription_rejected = stats.subscription_rejected.saturating_add(1);
            });
            return;
        }
        let generation = self.next_generation(&key);
        let sender = self.sender.clone();
        let future = (subscription.factory)(sender);
        let completion_key = key.clone();
        let abort = self.tasks.spawn(async move {
            future.await;
            SubscriptionCompletion {
                key: completion_key,
                generation,
            }
        });
        self.active
            .insert(key, ActiveSubscription { generation, abort });
        with_stats(&self.stats, |stats| {
            stats.subscriptions_started = stats.subscriptions_started.saturating_add(1);
        });
    }

    pub fn cancel(&mut self, key: &SubscriptionKey) -> bool {
        let Some(active) = self.active.remove(key) else {
            return false;
        };
        active.abort.abort();
        let _generation = self.next_generation(key);
        with_stats(&self.stats, |stats| {
            stats.subscriptions_cancelled = stats.subscriptions_cancelled.saturating_add(1);
        });
        true
    }

    pub async fn next_completion(&mut self) {
        while let Some(result) = self.tasks.join_next().await {
            let Ok(completion) = result else {
                continue;
            };
            self.accept_completion(&completion);
            return;
        }
    }

    pub fn try_next_completion(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            let Ok(completion) = result else {
                continue;
            };
            self.accept_completion(&completion);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn accept_completion(&mut self, completion: &SubscriptionCompletion) {
        if self
            .active
            .get(&completion.key)
            .is_some_and(|active| active.generation == completion.generation)
        {
            self.active.remove(&completion.key);
            with_stats(&self.stats, |stats| {
                stats.subscriptions_completed = stats.subscriptions_completed.saturating_add(1);
            });
        }
    }

    pub async fn shutdown(&mut self) {
        let cancelled = u64::try_from(self.active.len()).unwrap_or(u64::MAX);
        self.tasks.shutdown().await;
        self.active.clear();
        if cancelled > 0 {
            with_stats(&self.stats, |stats| {
                stats.subscriptions_cancelled =
                    stats.subscriptions_cancelled.saturating_add(cancelled);
            });
        }
    }

    fn next_generation(&mut self, key: &SubscriptionKey) -> u64 {
        let generation = self.generations.entry(key.clone()).or_default();
        *generation = generation.wrapping_add(1);
        *generation
    }
}
