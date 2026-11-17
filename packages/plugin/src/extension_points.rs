//! Host-declared extension points.
//!
//! A host program declares the extension points it offers
//! ([`ExtensionPoints::declare`], typed by a BPDL-generated
//! `bmux_plugin_sdk::author::ExtensionPoint`). Plugins contribute through
//! `Declarations::extend`, and the host collects the contributions from
//! each instance's registrations in both packaging modes
//! ([`ExtensionPoints::contribute_registrations`]). Consumers read an
//! [`ExtensionSet`]: a sorted snapshot plus a change stream, whose entries
//! call the contributor's behavior through the instance's extension port.
//!
//! Nothing here knows what any point means. Ordering uses the point's
//! `order` with the plugin id as a stable tiebreak. A contribution to an
//! undeclared point, data that does not decode, and a duplicate key are
//! rejected, and the error names the plugins involved.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use bmux_plugin_sdk::author::ExtensionPoint;
use bmux_plugin_sdk::definition::{InstanceRegistrations, PluginFuture};
use bmux_plugin_sdk::{PluginContribution, PluginError};
use tokio::sync::watch;

fn failure(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}

/// Calls one plugin instance's contribution behavior (its extension port).
pub trait ExtensionInvoker: Send + Sync {
    /// Run one encoded behavior request against `extension_point`.
    fn invoke(
        &self,
        extension_point: &str,
        payload: Vec<u8>,
    ) -> PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>>;
}

/// Decoded ordering facts of one contribution, from the point's type.
type Inspect = fn(&[u8]) -> Result<(i64, Option<String>), String>;

fn inspect<E: ExtensionPoint>(data: &[u8]) -> Result<(i64, Option<String>), String> {
    let data: E::Data =
        bmux_plugin_sdk::decode_service_message(data).map_err(|error| error.to_string())?;
    Ok((E::order(&data), E::key(&data).map(str::to_owned)))
}

#[derive(Clone)]
struct Entry {
    plugin_id: String,
    order: i64,
    key: Option<String>,
    data: Vec<u8>,
    invoker: Arc<dyn ExtensionInvoker>,
}

struct Declared {
    version: u32,
    inspect: Inspect,
    entries: watch::Sender<Arc<[Entry]>>,
}

/// The extension points a host declares, and their contributions.
#[derive(Default)]
pub struct ExtensionPoints {
    points: Mutex<BTreeMap<&'static str, Declared>>,
}

impl ExtensionPoints {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer extension point `E`.
    ///
    /// # Errors
    /// `E` is already declared.
    pub fn declare<E: ExtensionPoint>(&self) -> bmux_plugin_sdk::Result<()> {
        let mut points = self
            .points
            .lock()
            .map_err(|_| failure("extension points poisoned"))?;
        if points.contains_key(E::ID) {
            return Err(failure(format!(
                "extension point '{}' is declared twice",
                E::ID
            )));
        }
        points.insert(
            E::ID,
            Declared {
                version: E::VERSION,
                inspect: inspect::<E>,
                entries: watch::channel(Arc::from(Vec::new())).0,
            },
        );
        drop(points);
        Ok(())
    }

    /// Whether `id` is a declared extension point.
    #[must_use]
    pub fn declares(&self, id: &str) -> bool {
        self.points
            .lock()
            .is_ok_and(|points| points.contains_key(id))
    }

    /// Accept one contribution of `plugin_id`.
    ///
    /// # Errors
    /// The point is not declared, the data does not decode as its `Data`,
    /// or another contribution already holds its key.
    pub fn contribute(
        &self,
        plugin_id: &str,
        extension_point: &str,
        data: &[u8],
        invoker: Arc<dyn ExtensionInvoker>,
    ) -> bmux_plugin_sdk::Result<()> {
        let points = self
            .points
            .lock()
            .map_err(|_| failure("extension points poisoned"))?;
        let point = points.get(extension_point).ok_or_else(|| {
            failure(format!(
                "plugin '{plugin_id}' contributes to '{extension_point}', which this host does not declare"
            ))
        })?;
        let (order, key) = (point.inspect)(data).map_err(|error| {
            failure(format!(
                "plugin '{plugin_id}' contributes data to '{extension_point}' (v{}) that does not decode: {error}",
                point.version
            ))
        })?;
        let current = point.entries.borrow().clone();
        if let Some(key) = &key
            && let Some(holder) = current.iter().find(|entry| entry.key.as_ref() == Some(key))
        {
            return Err(failure(format!(
                "plugin '{plugin_id}' contributes '{key}' to '{extension_point}', which plugin '{}' already contributes",
                holder.plugin_id
            )));
        }
        let mut entries = current.to_vec();
        entries.push(Entry {
            plugin_id: plugin_id.to_owned(),
            order,
            key,
            data: data.to_vec(),
            invoker,
        });
        entries.sort_by(|left, right| {
            (left.order, &left.plugin_id).cmp(&(right.order, &right.plugin_id))
        });
        point.entries.send_replace(Arc::from(entries));
        drop(points);
        Ok(())
    }

    /// Accept every extension contribution in one instance's registrations,
    /// all or nothing.
    ///
    /// # Errors
    /// As [`Self::contribute`]; contributions accepted before the failure
    /// are withdrawn.
    pub fn contribute_registrations(
        &self,
        plugin_id: &str,
        registrations: &InstanceRegistrations,
        invoker: &Arc<dyn ExtensionInvoker>,
    ) -> bmux_plugin_sdk::Result<()> {
        for contribution in &registrations.contributions {
            if let PluginContribution::Extension {
                extension_point,
                payload,
                ..
            } = contribution
                && let Err(error) =
                    self.contribute(plugin_id, extension_point, payload, Arc::clone(invoker))
            {
                self.withdraw(plugin_id);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Remove every contribution of `plugin_id` (its instance went away).
    pub fn withdraw(&self, plugin_id: &str) {
        let Ok(points) = self.points.lock() else {
            return;
        };
        for point in points.values() {
            point.entries.send_if_modified(|entries| {
                if !entries.iter().any(|entry| entry.plugin_id == plugin_id) {
                    return false;
                }
                *entries = entries
                    .iter()
                    .filter(|entry| entry.plugin_id != plugin_id)
                    .cloned()
                    .collect();
                true
            });
        }
    }

    /// The contributions to `E`.
    ///
    /// # Errors
    /// `E` is not declared.
    pub fn set<E: ExtensionPoint>(&self) -> bmux_plugin_sdk::Result<ExtensionSet<E>> {
        let points = self
            .points
            .lock()
            .map_err(|_| failure("extension points poisoned"))?;
        let point = points
            .get(E::ID)
            .ok_or_else(|| failure(format!("extension point '{}' is not declared", E::ID)))?;
        let receiver = point.entries.subscribe();
        drop(points);
        Ok(ExtensionSet {
            receiver,
            point: PhantomData,
        })
    }
}

/// The contributions to one extension point: a sorted snapshot and a change
/// stream.
pub struct ExtensionSet<E> {
    receiver: watch::Receiver<Arc<[Entry]>>,
    point: PhantomData<fn() -> E>,
}

impl<E: ExtensionPoint> ExtensionSet<E> {
    /// The current contributions, in order.
    ///
    /// # Errors
    /// A contribution's data no longer decodes (it was checked on entry).
    pub fn snapshot(&self) -> bmux_plugin_sdk::Result<Vec<Extension<E>>> {
        let entries = self.receiver.borrow().clone();
        entries
            .iter()
            .map(|entry| {
                Ok(Extension {
                    plugin_id: entry.plugin_id.clone(),
                    data: bmux_plugin_sdk::decode_service_message(&entry.data)?,
                    invoker: Arc::clone(&entry.invoker),
                })
            })
            .collect()
    }

    /// Wait until the contributions change; `false` once the host dropped
    /// the point.
    pub async fn changed(&mut self) -> bool {
        self.receiver.changed().await.is_ok()
    }
}

/// One contribution: who made it, its declarative data, and its behavior.
pub struct Extension<E: ExtensionPoint> {
    pub plugin_id: String,
    pub data: E::Data,
    invoker: Arc<dyn ExtensionInvoker>,
}

impl<E: ExtensionPoint> Extension<E> {
    /// Call the contribution's behavior with one request of the point's
    /// generated `Request` type, decoding the response as `R`.
    ///
    /// # Errors
    /// The request does not encode, the instance failed, or the response
    /// does not decode.
    pub fn call<R: serde::de::DeserializeOwned>(
        &self,
        request: &impl serde::Serialize,
    ) -> impl std::future::Future<Output = bmux_plugin_sdk::Result<R>> + Send + 'static {
        let payload = bmux_plugin_sdk::encode_service_message(request);
        let invoker = Arc::clone(&self.invoker);
        async move {
            let response = invoker.invoke(E::ID, payload?).await?;
            bmux_plugin_sdk::decode_service_message(&response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Item {
        id: String,
        order: u32,
    }

    struct Point;
    impl ExtensionPoint for Point {
        const ID: &'static str = "host.item/v1";
        const VERSION: u32 = 1;
        type Data = Item;
        fn order(data: &Item) -> i64 {
            i64::from(data.order)
        }
        fn key(data: &Item) -> Option<&str> {
            Some(&data.id)
        }
    }

    /// Answers with the plugin's own name.
    struct Echo(&'static str);
    impl ExtensionInvoker for Echo {
        fn invoke(
            &self,
            extension_point: &str,
            _: Vec<u8>,
        ) -> PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>> {
            let reply = format!("{}@{extension_point}", self.0);
            Box::pin(async move { bmux_plugin_sdk::encode_service_message(&reply) })
        }
    }

    fn data(id: &str, order: u32) -> Vec<u8> {
        bmux_plugin_sdk::encode_service_message(&Item {
            id: id.into(),
            order,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn contributions_sort_reject_duplicates_and_withdraw_with_their_plugin() {
        let points = ExtensionPoints::new();
        assert!(points.set::<Point>().is_err(), "undeclared");
        points.declare::<Point>().unwrap();
        assert!(points.declare::<Point>().is_err());
        let mut set = points.set::<Point>().unwrap();
        points
            .contribute(
                "b.plugin",
                Point::ID,
                &data("clock", 10),
                Arc::new(Echo("b")),
            )
            .unwrap();
        points
            .contribute(
                "a.plugin",
                Point::ID,
                &data("load", 10),
                Arc::new(Echo("a")),
            )
            .unwrap();
        points
            .contribute(
                "c.plugin",
                Point::ID,
                &data("first", 1),
                Arc::new(Echo("c")),
            )
            .unwrap();
        assert!(set.changed().await);
        let order = set
            .snapshot()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.plugin_id, entry.data.id))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            [
                ("c.plugin".to_owned(), "first".to_owned()),
                ("a.plugin".to_owned(), "load".to_owned()),
                ("b.plugin".to_owned(), "clock".to_owned()),
            ]
        );
        let duplicate = points
            .contribute(
                "d.plugin",
                Point::ID,
                &data("clock", 5),
                Arc::new(Echo("d")),
            )
            .unwrap_err()
            .to_string();
        assert!(
            duplicate.contains("d.plugin") && duplicate.contains("b.plugin"),
            "{duplicate}"
        );
        assert!(
            points
                .contribute(
                    "e.plugin",
                    "host.other/v1",
                    &data("x", 0),
                    Arc::new(Echo("e"))
                )
                .is_err()
        );
        assert!(
            points
                .contribute("e.plugin", Point::ID, b"not data", Arc::new(Echo("e")))
                .is_err()
        );
        let first = &set.snapshot().unwrap()[0];
        let reply: String = first.call(&()).await.unwrap();
        assert_eq!(reply, "c@host.item/v1");
        points.withdraw("c.plugin");
        assert_eq!(set.snapshot().unwrap().len(), 2);
    }
}
