//! Version-independent derived indexes are rebuilt, never trusted from disk.
use super::*;
impl ExchangeState {
    pub(crate) fn restore_snapshot_indexes(&mut self) -> Result<(), String> {
        self.order_locations.clear();
        self.perp_resting_exposure.clear();
        for (&market, book) in self.order_books.iter_mut() {
            book.rebuild_snapshot_indexes();
            for order in book.bids.values().chain(book.asks.values()).flatten() {
                if order.market != market
                    || order.remaining_quantity == 0
                    || self.order_locations.contains_key(&order.id)
                {
                    return Err("invalid snapshot order".into());
                }
                self.order_locations.insert(
                    order.id,
                    OrderLocation {
                        market,
                        side: order.side,
                        price: order.price,
                    },
                );
                if matches!(self.markets[&market].market_type, MarketType::Perp { .. }) {
                    self.perp_resting_exposure
                        .entry((market, order.account))
                        .or_default()
                        .change(order.side, order.remaining_quantity, true)
                        .map_err(|e| format!("{e:?}"))?;
                }
            }
        }
        self.rebuild_state_tree();
        Ok(())
    }
}
pub(super) mod map_entries {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::hash::Hash;
    pub fn serialize<K: Serialize + Eq + Hash + Clone, V: Serialize + Clone, S: Serializer>(
        map: &im::HashMap<K, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        map.iter().collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<
        'de,
        K: Deserialize<'de> + Eq + Hash + Clone,
        V: Deserialize<'de> + Clone,
        D: Deserializer<'de>,
    >(
        d: D,
    ) -> Result<im::HashMap<K, V>, D::Error> {
        Ok(Vec::<(K, V)>::deserialize(d)?.into_iter().collect())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn populated_snapshot_restores_orders_balances_and_cross_collateral() {
        let mut original = ExchangeState::with_claim_faucet(FaucetConfig {
            max_claims: 2,
            usd_per_claim: 1000 * USD_ATOMS,
            btc_per_claim: BTC_ATOMS,
        })
        .unwrap();
        for id in 0..2 {
            original
                .execute(&Transaction::ClaimAirdrop {
                    account_id: id.into(),
                    nonce: 0,
                })
                .unwrap();
        }
        original
            .execute(&Transaction::PlaceOrder {
                account_id: 0.into(),
                market_id: 2,
                order_type: OrderType::LO,
                side: Side::Sell,
                price: 10000,
                quantity: 10000,
                nonce: 1,
            })
            .unwrap();
        original
            .execute(&Transaction::PlaceOrder {
                account_id: 1.into(),
                market_id: 2,
                order_type: OrderType::IOC,
                side: Side::Buy,
                price: 10000,
                quantity: 5000,
                nonce: 1,
            })
            .unwrap();
        original
            .execute(&Transaction::SetMarginMode {
                account_id: 0.into(),
                mode: perp::MarginMode::Cross,
                nonce: 2,
            })
            .unwrap();
        original
            .execute(&Transaction::DepositPerpCollateral {
                account_id: 0.into(),
                market_id: 1,
                amount: 500 * USD_ATOMS,
                nonce: 3,
            })
            .unwrap();
        let order_id = original.account_orders(0.into())[0].id;
        let bytes = serde_json::to_vec(&original).unwrap();
        let mut restored: ExchangeState = serde_json::from_slice(&bytes).unwrap();
        restored.restore_snapshot_indexes().unwrap();
        assert_eq!(original.state_digest(), restored.state_digest());
        let cancel = Transaction::CancelOrder {
            account_id: 0.into(),
            market_id: 2,
            order_id,
            nonce: 4,
        };
        original.execute(&cancel).unwrap();
        restored.execute(&cancel).unwrap();
        assert_eq!(original.state_digest(), restored.state_digest());
        assert!(restored.verify_state_tree());
        assert_eq!(restored.open_order_count(), 0);
    }
    #[test]
    fn exchange_snapshot_rebuilds_canonical_root() {
        let original = ExchangeState::default();
        let bytes = serde_json::to_vec(&original).unwrap();
        let mut restored: ExchangeState = serde_json::from_slice(&bytes).unwrap();
        restored.restore_snapshot_indexes().unwrap();
        assert_eq!(original.state_digest(), restored.state_digest());
        assert!(restored.verify_state_tree());
    }
}
