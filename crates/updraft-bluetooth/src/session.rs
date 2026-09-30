#[cfg(any(target_os = "linux", test))]
use std::{collections::HashMap, future::Future};

#[cfg(any(target_os = "linux", test))]
use tokio::{runtime::Id as RuntimeId, sync::Mutex, task::JoinHandle};

#[path = "request_session.rs"]
mod request_session;

#[cfg(not(target_os = "linux"))]
#[path = "btleplug_session.rs"]
mod implementation;

#[cfg(target_os = "linux")]
#[path = "linux_session.rs"]
mod implementation;

pub(super) use implementation::query_peripheral;

#[cfg(any(target_os = "linux", test))]
pub(super) struct SessionEntry<T> {
    session: T,
    driver: JoinHandle<()>,
}

#[cfg(any(target_os = "linux", test))]
pub(super) struct RuntimeSessionCache<T> {
    entries: Mutex<HashMap<RuntimeId, SessionEntry<T>>>,
}

#[cfg(any(target_os = "linux", test))]
impl<T> Default for RuntimeSessionCache<T> {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl<T: Clone> RuntimeSessionCache<T> {
    pub(super) async fn get_or_init<E, F, Fut>(
        &self,
        runtime_id: RuntimeId,
        create: F,
    ) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(T, JoinHandle<()>), E>>,
    {
        let mut entries = self.entries.lock().await;
        entries.retain(|id, entry| *id == runtime_id || !entry.driver.is_finished());
        if let Some(entry) = entries.remove(&runtime_id)
            && !entry.driver.is_finished()
        {
            let session = entry.session.clone();
            entries.insert(runtime_id, entry);
            return Ok(session);
        }

        let (session, driver) = create().await?;
        entries.insert(
            runtime_id,
            SessionEntry {
                session: session.clone(),
                driver,
            },
        );
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use std::future;

    use super::*;

    #[tokio::test]
    async fn runtime_session_cache_reuses_and_replaces_driver_sessions() {
        let cache = RuntimeSessionCache::default();
        let runtime_id = tokio::runtime::Handle::current().id();
        let mut creations = 0;

        let first = cache
            .get_or_init(runtime_id, || async {
                creations += 1;
                Ok::<_, anyhow::Error>((creations, tokio::spawn(future::pending::<()>())))
            })
            .await
            .unwrap();
        let reused = cache
            .get_or_init(runtime_id, || async {
                creations += 1;
                Ok::<_, anyhow::Error>((creations, tokio::spawn(future::pending::<()>())))
            })
            .await
            .unwrap();
        assert_eq!(first, reused);
        assert_eq!(creations, 1);

        cache
            .entries
            .lock()
            .await
            .get(&runtime_id)
            .unwrap()
            .driver
            .abort();
        tokio::task::yield_now().await;
        let replacement = cache
            .get_or_init(runtime_id, || async {
                creations += 1;
                Ok::<_, anyhow::Error>((creations, tokio::spawn(future::pending::<()>())))
            })
            .await
            .unwrap();
        assert_ne!(first, replacement);
        assert_eq!(creations, 2);

        let other_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let other_runtime_id = other_runtime.handle().id();
        let other_driver = other_runtime.spawn(future::pending::<()>());
        cache
            .get_or_init(other_runtime_id, || async {
                Ok::<_, anyhow::Error>((3, other_driver))
            })
            .await
            .unwrap();
        other_runtime.shutdown_background();

        let same_runtime = cache
            .get_or_init(runtime_id, || async {
                creations += 1;
                Ok::<_, anyhow::Error>((creations, tokio::spawn(future::pending::<()>())))
            })
            .await
            .unwrap();
        assert_eq!(same_runtime, replacement);
        let entries = cache.entries.lock().await;
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key(&runtime_id));
    }
}
