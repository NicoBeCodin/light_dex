//! Legacy controller status remains supported, but projection, HMAC/JSON and
//! socket writes run on one bounded worker, never the consensus owner.
use super::status::AssetBalanceSnapshot;
use super::*;
pub(super) struct StatusWriter(mpsc::SyncSender<Job>);
struct Job {
    status: NodeStatus,
    snapshot: Option<Arc<read_api::ReadSnapshot>>,
    owners: Option<Vec<Address>>,
    stream: TcpStream,
}
impl StatusWriter {
    pub fn new(key: [u8; 32]) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Job>(2);
        thread::Builder::new()
            .name("status-writer".into())
            .spawn(move || {
                while let Ok(mut job) = receiver.recv() {
                    if let Some(snapshot) = &job.snapshot {
                        populate(&mut job.status, snapshot, job.owners.as_deref());
                    }
                    if let Ok(line) = sign_line(&job.status, &key) {
                        let _ = job.stream.set_write_timeout(Some(Duration::from_secs(3)));
                        let _ = job.stream.write_all(line.as_bytes());
                    }
                }
            })
            .expect("status writer");
        Self(sender)
    }
    pub fn send(
        &self,
        status: NodeStatus,
        snapshot: Arc<read_api::ReadSnapshot>,
        owners: Option<Vec<Address>>,
        stream: TcpStream,
    ) {
        let _ = self.0.try_send(Job {
            status,
            snapshot: Some(snapshot),
            owners,
            stream,
        });
    }
    pub fn send_health(&self, status: NodeStatus, stream: TcpStream) {
        let _ = self.0.try_send(Job {
            status,
            snapshot: None,
            owners: None,
            stream,
        });
    }
}

fn populate(
    status: &mut NodeStatus,
    snapshot: &read_api::ReadSnapshot,
    owners: Option<&[Address]>,
) {
    let exchange = &snapshot.exchange;
    status.perp_risk = exchange.perp_risk_snapshots();
    status.perp_positions = exchange.perp_position_snapshots();
    status.account_nonces = exchange
        .accounts()
        .values()
        .map(|account| AccountNonce {
            account_id: account.id,
            nonce: account.nonce,
            used_nonces: account.used_nonces,
        })
        .collect();
    status.agent_balances = exchange
        .accounts()
        .values()
        .map(|account| {
            let usd = account.balances.get(&crate::exchange::market::USD);
            let btc = account.balances.get(&crate::exchange::market::BTC);
            let mut assets: Vec<_> = account
                .balances
                .iter()
                .map(|(&asset_id, balance)| AssetBalanceSnapshot {
                    asset_id,
                    available: balance.available,
                    locked: balance.locked,
                })
                .collect();
            assets.sort_by_key(|asset| asset.asset_id);
            AgentBalance {
                account_id: account.id,
                assets,
                usd_available: usd.map_or(0, |b| b.available),
                usd_locked: usd.map_or(0, |b| b.locked),
                btc_available: btc.map_or(0, |b| b.available),
                btc_locked: btc.map_or(0, |b| b.locked),
            }
        })
        .collect();
    status.recent_transaction_outcomes = snapshot.outcomes.clone();
    status.recent_trades = snapshot.trades.iter().rev().cloned().collect();
    status.order_books = exchange
        .order_books()
        .iter()
        .map(|(&id, book)| book.snapshot(id, 20))
        .collect();
    status.order_books.sort_by_key(|book| book.market_id);
    let make = |order: &crate::exchange::order::Order| OpenOrderSnapshot {
        order_id: order.id,
        account_id: order.account,
        market_id: order.market,
        side: order.side,
        price: order.price,
        remaining_quantity: order.remaining_quantity,
    };
    status.open_orders = if let Some(owners) = owners {
        owners
            .iter()
            .flat_map(|owner| exchange.account_orders(*owner))
            .map(make)
            .collect()
    } else {
        exchange
            .order_books()
            .values()
            .flat_map(|book| book.bids.values().chain(book.asks.values()).flatten())
            .map(make)
            .collect()
    };
    status.open_orders.sort_by_key(|order| order.order_id);
    status.open_orders.dedup_by_key(|order| order.order_id);
    status.open_orders_complete = owners.is_none();
}
