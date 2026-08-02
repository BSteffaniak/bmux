//! Long-lived application subscription factories.

use std::future::Future;
use std::pin::Pin;

use crate::ids::SubscriptionKey;
use crate::runtime::SubscriptionSender;

/// Boxed long-lived subscription future.
pub type SubscriptionFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Application-owned long-lived message producer.
pub struct Subscription<M> {
    pub(crate) key: SubscriptionKey,
    pub(crate) factory: Box<dyn FnOnce(SubscriptionSender<M>) -> SubscriptionFuture + Send>,
}

impl<M: Send + 'static> Subscription<M> {
    /// Create a subscription whose future receives a bounded reliable sender.
    #[must_use]
    pub fn new<F, Fut>(key: SubscriptionKey, run: F) -> Self
    where
        F: FnOnce(SubscriptionSender<M>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            key,
            factory: Box::new(move |sender| Box::pin(run(sender))),
        }
    }
}
