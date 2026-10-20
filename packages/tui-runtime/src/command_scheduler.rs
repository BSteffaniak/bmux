//! Runtime-owned command scheduling.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::task::{AbortHandle, JoinSet};

use crate::command::{Command, CommandFuture, CommandPolicy};
use crate::ids::CommandKey;
use crate::mailbox::with_stats;
use crate::stats::{QueueSource, RuntimeStats};

pub struct CommandCompletion<M> {
    pub key: Option<CommandKey>,
    pub generation: u64,
    pub message: Option<M>,
    ready_at: Option<Instant>,
}

struct ActiveCommand {
    generation: u64,
    abort: AbortHandle,
}

struct QueuedCommand<M> {
    generation: u64,
    future: CommandFuture<M>,
}

pub struct CommandScheduler<M> {
    tasks: JoinSet<CommandCompletion<M>>,
    active_keyed: BTreeMap<CommandKey, ActiveCommand>,
    queued_latest: BTreeMap<CommandKey, QueuedCommand<M>>,
    generations: BTreeMap<CommandKey, u64>,
    active_count: usize,
    max_active: usize,
    max_queued: usize,
    stats: Arc<Mutex<RuntimeStats>>,
    detailed: bool,
}

impl<M: Send + 'static> CommandScheduler<M> {
    pub fn new(
        max_active: usize,
        max_queued: usize,
        stats: Arc<Mutex<RuntimeStats>>,
        detailed: bool,
    ) -> Self {
        Self {
            tasks: JoinSet::new(),
            active_keyed: BTreeMap::new(),
            queued_latest: BTreeMap::new(),
            generations: BTreeMap::new(),
            active_count: 0,
            max_active,
            max_queued,
            stats,
            detailed,
        }
    }

    pub fn schedule(&mut self, command: Command<M>) {
        match command.policy {
            CommandPolicy::Concurrent => {
                let Some(future) = command.future else {
                    return;
                };
                if self.active_count >= self.max_active {
                    self.reject();
                } else {
                    self.spawn(None, 0, future);
                }
            }
            CommandPolicy::StartIfIdle(key) => {
                let Some(future) = command.future else {
                    return;
                };
                if self.active_keyed.contains_key(&key) || self.queued_latest.contains_key(&key) {
                    self.reject();
                } else if self.active_count >= self.max_active {
                    self.queue_if_available(key, future, false);
                } else {
                    let generation = self.next_generation(&key);
                    self.spawn(Some(key), generation, future);
                }
            }
            CommandPolicy::Replace(key) => {
                let Some(future) = command.future else {
                    return;
                };
                self.cancel_key(&key);
                let generation = self.next_generation(&key);
                if self.active_count >= self.max_active {
                    self.insert_queued(key, generation, future, false);
                } else {
                    self.spawn(Some(key), generation, future);
                }
            }
            CommandPolicy::QueueLatest(key) => {
                let Some(future) = command.future else {
                    return;
                };
                if self.active_keyed.contains_key(&key) || self.active_count >= self.max_active {
                    self.queue_if_available(key, future, true);
                } else {
                    let generation = self.next_generation(&key);
                    self.spawn(Some(key), generation, future);
                }
            }
            CommandPolicy::Cancel(key) => self.cancel_key(&key),
        }
    }

    fn queue_if_available(&mut self, key: CommandKey, future: CommandFuture<M>, latest: bool) {
        if self.queued_latest.contains_key(&key) {
            if latest {
                let generation = self.next_generation(&key);
                self.insert_queued(key, generation, future, true);
            } else {
                self.reject();
            }
        } else if self.queued_latest.len() >= self.max_queued {
            self.reject();
        } else {
            let generation = self.next_generation(&key);
            self.insert_queued(key, generation, future, false);
        }
    }

    fn insert_queued(
        &mut self,
        key: CommandKey,
        generation: u64,
        future: CommandFuture<M>,
        replacement: bool,
    ) {
        if !replacement && self.queued_latest.len() >= self.max_queued {
            self.reject();
            return;
        }
        self.queued_latest
            .insert(key, QueuedCommand { generation, future });
        with_stats(&self.stats, |stats| {
            stats.commands_queued = stats.commands_queued.saturating_add(1);
        });
    }

    fn spawn(&mut self, key: Option<CommandKey>, generation: u64, future: CommandFuture<M>) {
        let completion_key = key.clone();
        let detailed = self.detailed;
        let abort = self.tasks.spawn(async move {
            let message = future.await;
            CommandCompletion {
                key: completion_key,
                generation,
                message,
                ready_at: detailed.then(Instant::now),
            }
        });
        self.active_count = self.active_count.saturating_add(1);
        if let Some(key) = key {
            self.active_keyed
                .insert(key, ActiveCommand { generation, abort });
        }
        with_stats(&self.stats, |stats| {
            stats.commands_started = stats.commands_started.saturating_add(1);
        });
    }

    pub async fn next_completion(&mut self) -> Option<CommandCompletion<M>> {
        loop {
            let result = self.tasks.join_next().await?;
            self.active_count = self.active_count.saturating_sub(1);
            let Ok(completion) = result else {
                continue;
            };
            if let Some(ready_at) = completion.ready_at {
                let collected_at = Instant::now();
                with_stats(&self.stats, |stats| {
                    stats.record_queue_age(QueueSource::CommandCompletion, ready_at, collected_at);
                });
            }
            if let Some(key) = &completion.key {
                let current = self.active_keyed.get(key).map(|active| active.generation);
                if current != Some(completion.generation) {
                    with_stats(&self.stats, |stats| {
                        stats.stale_command_completions =
                            stats.stale_command_completions.saturating_add(1);
                    });
                    self.start_queued_work();
                    continue;
                }
                self.active_keyed.remove(key);
            }
            self.start_queued_work();
            return Some(completion);
        }
    }

    pub fn try_next_completion(&mut self) -> Option<CommandCompletion<M>> {
        loop {
            let result = self.tasks.try_join_next()?;
            self.active_count = self.active_count.saturating_sub(1);
            let Ok(completion) = result else {
                continue;
            };
            if let Some(ready_at) = completion.ready_at {
                let collected_at = Instant::now();
                with_stats(&self.stats, |stats| {
                    stats.record_queue_age(QueueSource::CommandCompletion, ready_at, collected_at);
                });
            }
            if let Some(key) = &completion.key {
                let current = self.active_keyed.get(key).map(|active| active.generation);
                if current != Some(completion.generation) {
                    with_stats(&self.stats, |stats| {
                        stats.stale_command_completions =
                            stats.stale_command_completions.saturating_add(1);
                    });
                    self.start_queued_work();
                    continue;
                }
                self.active_keyed.remove(key);
            }
            self.start_queued_work();
            return Some(completion);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn start_queued_work(&mut self) {
        while self.active_count < self.max_active {
            let Some((key, queued)) = self.queued_latest.pop_first() else {
                break;
            };
            if self.active_keyed.contains_key(&key) {
                self.queued_latest.insert(key, queued);
                break;
            }
            self.spawn(Some(key), queued.generation, queued.future);
        }
    }

    fn cancel_key(&mut self, key: &CommandKey) {
        let mut cancelled = 0_u64;
        if let Some(active) = self.active_keyed.remove(key) {
            active.abort.abort();
            cancelled = cancelled.saturating_add(1);
        }
        if self.queued_latest.remove(key).is_some() {
            cancelled = cancelled.saturating_add(1);
        }
        if cancelled > 0 {
            with_stats(&self.stats, |stats| {
                stats.commands_cancelled = stats.commands_cancelled.saturating_add(cancelled);
            });
        }
        let _generation = self.next_generation(key);
    }

    fn next_generation(&mut self, key: &CommandKey) -> u64 {
        let generation = self.generations.entry(key.clone()).or_default();
        *generation = generation.wrapping_add(1);
        *generation
    }

    fn reject(&self) {
        with_stats(&self.stats, |stats| {
            stats.commands_rejected = stats.commands_rejected.saturating_add(1);
        });
    }

    pub async fn shutdown(&mut self) {
        let cancelled = u64::try_from(self.tasks.len()).unwrap_or(u64::MAX)
            + u64::try_from(self.queued_latest.len()).unwrap_or(u64::MAX);
        self.tasks.shutdown().await;
        self.active_keyed.clear();
        self.queued_latest.clear();
        if cancelled > 0 {
            with_stats(&self.stats, |stats| {
                stats.commands_cancelled = stats.commands_cancelled.saturating_add(cancelled);
            });
        }
    }
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use crate::mailbox::stats_snapshot;
    use tokio::sync::oneshot;

    fn observations(stats: &Arc<Mutex<RuntimeStats>>) -> Option<u64> {
        stats_snapshot(stats)
            .queue_age(QueueSource::CommandCompletion)
            .map(|histogram| histogram.buckets.iter().sum())
    }

    #[tokio::test]
    async fn awaited_completion_records_some_and_none_once_before_message_filtering() {
        for detailed in [false, true] {
            for message in [Some(7_u64), None] {
                let stats = Arc::new(Mutex::new(RuntimeStats::new(detailed)));
                let mut scheduler = CommandScheduler::new(1, 1, Arc::clone(&stats), detailed);
                let (release, released) = oneshot::channel();
                scheduler.schedule(Command::concurrent(async move {
                    released.await.unwrap();
                    message
                }));
                // Exercise the actual blocking collection API used by Runtime::run's
                // select branch, with a future that cannot yet have completed.
                {
                    let wait = scheduler.next_completion();
                    tokio::pin!(wait);
                    assert!(
                        std::future::poll_fn(|cx| {
                            std::task::Poll::Ready(wait.as_mut().poll(cx).is_pending())
                        })
                        .await
                    );
                    release.send(()).unwrap();
                    let completion = wait.await.unwrap();
                    assert_eq!(completion.message, message);
                    assert_eq!(completion.ready_at.is_some(), detailed);
                }
                assert_eq!(observations(&stats), detailed.then_some(1));
                assert!(scheduler.next_completion().await.is_none());
                assert!(scheduler.try_next_completion().is_none());
                assert_eq!(observations(&stats), detailed.then_some(1));
            }
        }
    }

    #[tokio::test]
    async fn awaited_collection_counts_completed_stale_result_once_without_delivery() {
        for detailed in [false, true] {
            for message in [Some(9_u64), None] {
                let stats = Arc::new(Mutex::new(RuntimeStats::new(detailed)));
                let mut scheduler = CommandScheduler::new(1, 1, Arc::clone(&stats), detailed);
                let key = CommandKey::new("stale-before-wait");
                scheduler.schedule(Command::start_if_idle(key.clone(), async move { message }));
                // Complete through the real spawned command wrapper, then invalidate
                // its generation before collecting. No fabricated completion/timestamp.
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    while !scheduler.active_keyed[&key].abort.is_finished() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(observations(&stats), detailed.then_some(0));
                scheduler.schedule(Command::cancel(key));
                assert!(scheduler.next_completion().await.is_none());
                assert_eq!(observations(&stats), detailed.then_some(1));
                assert_eq!(stats_snapshot(&stats).stale_command_completions, 1);
                assert!(scheduler.try_next_completion().is_none());
                assert_eq!(observations(&stats), detailed.then_some(1));
            }
        }
    }
}
