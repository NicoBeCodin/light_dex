//! One projection per subscribed resource, shared by all its readers. Watch
//! channels retain only the newest full snapshot; sequence gaps need no patch
//! replay. A slow socket cannot backpressure consensus or other subscribers.
use super::*;
use std::{
    collections::BTreeSet,
    sync::{Mutex, Weak},
};
use tokio::sync::watch;

pub(super) struct Published {
    pub value: Value,
    pub text: Arc<str>,
    dependencies: BTreeSet<u64>,
}
impl Published {
    pub fn new(value: Value) -> Self {
        let dependencies = ["positions", "open_orders"]
            .into_iter()
            .flat_map(|key| {
                value
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|item| item.get("market_id").and_then(Value::as_u64))
            })
            .collect();
        Self {
            text: value.to_string().into(),
            value,
            dependencies,
        }
    }
}
type Update = Option<Result<Arc<Published>, StatusCode>>;
struct Topic {
    sender: watch::Sender<Update>,
}
#[derive(Clone, Default)]
pub(super) struct Topics(Arc<Mutex<HashMap<ReadQuery, Weak<Topic>>>>);

pub(super) async fn subscribe(
    state: ApiState,
    query: ReadQuery,
) -> Result<watch::Receiver<Update>, StatusCode> {
    let (topic, receiver) = {
        let mut topics = state.topics.0.lock().unwrap();
        if let Some(topic) = topics.get(&query).and_then(Weak::upgrade) {
            // A worker with zero readers may already be exiting. Never attach
            // to that dying generation; the new topic gets a fresh projection.
            if topic.sender.receiver_count() > 0 {
                return Ok(topic.sender.subscribe());
            }
        }
        topics.retain(|_, topic| topic.strong_count() != 0);
        if topics.len() >= 1024 {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let (sender, receiver) = watch::channel(None);
        let topic = Arc::new(Topic { sender });
        topics.insert(query, Arc::downgrade(&topic));
        (topic, receiver)
    };
    // Subscribe before projecting to avoid missing a commit during projection.
    let mut updates = state.updates.subscribe();
    tokio::spawn(async move {
        let mut dependencies = BTreeSet::new();
        let mut projected_height = 0;
        loop {
            let value = project(state.clone(), query).await;
            if let Ok(value) = &value {
                dependencies = value.dependencies.clone();
                projected_height = value
                    .value
                    .get("height")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
            if topic.sender.send(Some(value)).is_err() {
                break;
            }
            loop {
                tokio::select! {
                    _ = topic.sender.closed() => return,
                    update = updates.recv() => match update {
                        Ok(update) if update.height > projected_height && update.affects(query, &dependencies) => break,
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => break,
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    });
    Ok(receiver)
}

pub(super) async fn get(state: ApiState, query: ReadQuery) -> Result<Arc<Published>, StatusCode> {
    if matches!(query, ReadQuery::Summary) {
        return project(state, query).await;
    }
    let mut receiver = subscribe(state, query).await?;
    loop {
        if let Some(value) = receiver.borrow_and_update().clone() {
            return value;
        }
        receiver
            .changed()
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn subscribers_share_projection_and_unrelated_market_does_not_wake_them() {
        let (events, receiver) = crate::priority::channel(32);
        let (updates, _) = broadcast::channel(32);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = count.clone();
        let owner = std::thread::spawn(move || {
            while let Ok(NodeEvent::ReadQuery { reply, .. }) = receiver.recv() {
                let height = observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u64 + 1;
                let _ = reply.send(Arc::new(ReadSnapshot {
                    activity: serde_json::Value::Null,
                    node_id: 0,
                    height,
                    digest: None,
                    exchange: crate::exchange::ExchangeState::default(),
                    trades: vec![],
                    outcomes: vec![],
                    view: 0,
                    primary: 0,
                    mempool: 0,
                    peers: 0,
                    transactions: 0,
                    blocks: 0,
                }));
            }
        });
        let state = ApiState {
            events,
            updates: updates.clone(),
            auth: String::new(),
            topics: Topics::default(),
        };
        let mut first = subscribe(state.clone(), ReadQuery::Market(0))
            .await
            .unwrap();
        let mut second = subscribe(state.clone(), ReadQuery::Market(0))
            .await
            .unwrap();
        first.changed().await.unwrap();
        second.changed().await.unwrap();
        assert!(Arc::ptr_eq(
            first
                .borrow_and_update()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap(),
            second
                .borrow_and_update()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
        ));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        updates
            .send(Arc::new(ReadUpdate {
                height: 2,
                markets: [2].into(),
                ..Default::default()
            }))
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), first.changed())
                .await
                .is_err()
        );
        updates
            .send(Arc::new(ReadUpdate {
                height: 2,
                markets: [0].into(),
                ..Default::default()
            }))
            .unwrap();
        first.changed().await.unwrap();
        second.changed().await.unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(first);
        drop(second);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut third = subscribe(state.clone(), ReadQuery::Market(0))
            .await
            .unwrap();
        third.changed().await.unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
        drop(third);
        drop(state);
        drop(updates);
        tokio::time::sleep(Duration::from_millis(10)).await;
        owner.join().unwrap();
    }
}
