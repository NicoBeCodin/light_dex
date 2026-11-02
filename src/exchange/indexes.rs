//! Rebuildable read/risk indexes. They never enter the state commitment and
//! cannot grant authority: execution still checks canonical orders/accounts.
use super::*;

impl ExchangeState {
    pub fn open_order_count(&self) -> usize {
        self.order_locations.len()
    }
    pub(super) fn store_perp_account(
        &mut self,
        key: (MarketId, AccountId),
        account: IsolatedPerpAccount,
    ) {
        self.perp_markets_by_account
            .entry(key.1)
            .or_default()
            .insert(key.0);
        self.perp_accounts_by_market
            .entry(key.0)
            .or_default()
            .insert(key.1);
        self.perp_accounts.insert(key, account);
    }

    pub(super) fn remove_order_leaf(&mut self, id: OrderId) {
        self.state_tree.remove(StateKey::Order(id));
        if let Some(owner) = self.order_owners.remove(&id) {
            if let Some(orders) = self.owned_orders.get_mut(&owner) {
                orders.remove(&id);
                if orders.is_empty() {
                    self.owned_orders.remove(&owner);
                }
            }
        }
    }

    pub fn account_orders(&self, owner: AccountId) -> Vec<&Order> {
        self.owned_orders
            .get(&owner)
            .into_iter()
            .flatten()
            .filter_map(|id| {
                let location = self.order_locations.get(id)?;
                self.order_books.get(&location.market)?.find_order(
                    *id,
                    location.side,
                    location.price,
                    owner,
                )
            })
            .collect()
    }
}
