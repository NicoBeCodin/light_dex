//! Optional cache hints only: no execution, validation, allocation or mutation.
//! Kept off by default; extra lookups can cost more than the cache misses saved.
use super::*;

pub(crate) fn distance() -> usize {
    static DISTANCE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DISTANCE.get_or_init(|| {
        std::env::var("LIGHT_DEX_PREFETCH_DISTANCE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0)
            .min(64)
    })
}

fn hint<T>(value: &T) {
    #[cfg(target_arch = "x86_64")]
    #[allow(unused_unsafe)]
    // A non-owning cache hint. Pointer is borrowed from the immutable snapshot
    // and never retained; canonical execution performs all lookups again.
    unsafe {
        std::arch::x86_64::_mm_prefetch((value as *const T).cast(), std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::hint::black_box(value);
}

impl ExchangeState {
    pub fn prefetch_transaction(&self, tx: &Transaction) {
        if let Some(account) = self.accounts.get(&tx.account_id()) {
            hint(account);
        }
        if let Some(market) = tx.market_id() {
            if let Some(book) = self.order_books.get(&market) {
                hint(book);
            }
            if let Some(position) = self.perp_accounts.get(&(market, tx.account_id())) {
                hint(position);
            }
        }
        match tx {
            Transaction::Transfer { to, .. } => {
                if let Some(account) = self.accounts.get(to) {
                    hint(account);
                }
            }
            Transaction::CancelOrder { order_id, .. }
            | Transaction::ReduceOrder { order_id, .. } => self.prefetch_order(*order_id),
            Transaction::UpdateQuotes { cancel, reduce, .. } => {
                for &id in cancel.iter().take(4) {
                    self.prefetch_order(id);
                }
                for action in reduce.iter().take(4) {
                    self.prefetch_order(action.order_id);
                }
            }
            _ => {}
        }
    }

    fn prefetch_order(&self, id: OrderId) {
        if let Some(location) = self.order_locations.get(&id) {
            hint(location);
            if let Some(book) = self.order_books.get(&location.market) {
                if let Some(owner) = self.order_owners.get(&id) {
                    if let Some(order) = book.find_order(id, location.side, location.price, *owner)
                    {
                        hint(order);
                    }
                }
            }
        }
    }
}
