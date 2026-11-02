//! Atomic single-signer quote updates. Persistent forks stage only changed
//! paths. We deliberately preserve per-fill fees/rounding and FIFO semantics.
use super::*;
use crate::blockchain::{QuoteOrder, QuoteReduction};
mod amend;

impl ExchangeState {
    pub(super) fn update_quotes(
        &mut self,
        owner: AccountId,
        market: MarketId,
        cancel: &[OrderId],
        reduce: &[QuoteReduction],
        place: &[QuoteOrder],
        nonce: u64,
    ) -> Result<Vec<ExecutedTrade>, ExchangeError> {
        self.check_nonce(owner, nonce)?;
        let perp = matches!(
            self.markets
                .get(&market)
                .ok_or(ExchangeError::MarketNotFound)?
                .market_type,
            MarketType::Perp { .. }
        );
        let mut staged = self.fork();
        let nonce_checkpoint = {
            let account = staged.accounts.get(&owner).expect("checked account");
            (account.nonce, account.used_nonces)
        };
        let own_batch = !staged.state_tree.is_batching();
        if own_batch {
            staged.begin_state_batch();
        }
        let result = (|| {
            let mut fills = Vec::new();
            for &id in cancel {
                staged.charge_work(1)?;
                // Idempotent cancellations tolerate a fill between planning
                // and inclusion; a live foreign/wrong-market ID is not ignored.
                if !staged.order_locations.contains_key(&id) {
                    continue;
                }
                let account = staged.accounts.get_mut(&owner).expect("checked account");
                (account.nonce, account.used_nonces) = nonce_checkpoint;
                fills.extend(staged.execute_inner(&Transaction::CancelOrder {
                    account_id: owner,
                    market_id: market,
                    order_id: id,
                    nonce,
                })?);
            }
            for action in reduce {
                let account = staged.accounts.get_mut(&owner).expect("checked account");
                (account.nonce, account.used_nonces) = nonce_checkpoint;
                staged.execute_inner(&Transaction::ReduceOrder {
                    account_id: owner,
                    market_id: market,
                    order_id: action.order_id,
                    remaining_quantity: action.remaining_quantity,
                    nonce,
                })?;
            }
            for order in place {
                if !perp && order.reduce_only {
                    return Err(ExchangeError::InvalidOrder);
                }
                let account = staged.accounts.get_mut(&owner).expect("checked account");
                (account.nonce, account.used_nonces) = nonce_checkpoint;
                let tx = if perp {
                    Transaction::PlacePerpOrder {
                        account_id: owner,
                        market_id: market,
                        order_type: order.order_type,
                        side: order.side,
                        price: order.price,
                        quantity: order.quantity,
                        reduce_only: order.reduce_only,
                        nonce,
                    }
                } else {
                    Transaction::PlaceOrder {
                        account_id: owner,
                        market_id: market,
                        order_type: order.order_type,
                        side: order.side,
                        price: order.price,
                        quantity: order.quantity,
                        nonce,
                    }
                };
                fills.extend(staged.execute_inner(&tx)?);
            }
            let account = staged.accounts.get_mut(&owner).expect("checked account");
            (account.nonce, account.used_nonces) = nonce_checkpoint;
            account.consume_nonce(nonce);
            staged.state_tree.put_account(account);
            if own_batch {
                staged.finish_state_batch();
            }
            Ok(fills)
        })();
        // Failed work still counts against a proposal's deterministic budget.
        self.execution_work.set(staged.execution_work.get());
        let fills = result?;
        *self = staged;
        Ok(fills)
    }
}
