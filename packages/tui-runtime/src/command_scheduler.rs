//! Runtime-owned command scheduling.

use std::collections::BTreeMap;

use tokio::task::{AbortHandle, JoinSet};

use crate::command::{Command, CommandFuture, CommandPolicy};
use crate::ids::CommandKey;
use crate::mailbox::with_stats;
use crate::stats::RuntimeStats;
use std::sync::{Arc, Mutex};

pub struct CommandCompletion<M> {
    pub key: Option<CommandKey>,
    pub generation: u64,
    pub message: Option<M>,
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
}

impl<M: Send + 'static> CommandScheduler<M> {
    pub fn new(max_active: usize, max_queued: usize, stats: Arc<Mutex<RuntimeStats>>) -> Self {
        Self {
            tasks: JoinSet::new(),
            active_keyed: BTreeMap::new(),
            queued_latest: BTreeMap::new(),
            generations: BTreeMap::new(),
            active_count: 0,
            max_active,
            max_queued,
            stats,
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
        let abort = self.tasks.spawn(async move {
            let message = future.await;
            CommandCompletion {
                key: completion_key,
                generation,
                message,
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
