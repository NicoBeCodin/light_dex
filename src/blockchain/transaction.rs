use super::*;

/// One order action inside a signed, single-account/single-market quote batch.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, ts_rs::TS)]
pub struct QuoteOrder {
    pub order_type: OrderType,
    pub side: Side,
    pub price: u64,
    pub quantity: u64,
    pub reduce_only: bool,
}

/// A strict reduction of the quantity still resting at execution time.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, ts_rs::TS)]
pub struct QuoteReduction {
    pub order_id: OrderId,
    pub remaining_quantity: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, ts_rs::TS)]
pub enum Transaction {
    /// Atomic cancellations followed by placements, consuming exactly one
    /// nonce. Already-filled/cancelled IDs are no-ops; cancelling someone
    /// else's still-live order fails the whole batch. Unchanged orders retain
    /// their priority; replacements receive new IDs and queue priority.
    UpdateQuotes {
        account_id: AccountId,
        market_id: MarketId,
        cancel: Vec<OrderId>,
        reduce: Vec<QuoteReduction>,
        place: Vec<QuoteOrder>,
        nonce: u64,
    },
    ClaimAirdrop {
        account_id: AccountId,
        nonce: u64,
    },
    ReduceOrder {
        account_id: AccountId,
        market_id: MarketId,
        order_id: OrderId,
        remaining_quantity: u64,
        nonce: u64,
    },
    Transfer {
        from: AccountId,
        to: AccountId,
        asset: AssetId,
        amount: u64,
        nonce: u64,
    },

    PlaceOrder {
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        nonce: u64,
    },

    PlacePerpOrder {
        account_id: AccountId,
        market_id: MarketId,
        order_type: OrderType,
        side: Side,
        price: u64,
        quantity: u64,
        reduce_only: bool,
        nonce: u64,
    },

    CancelOrder {
        account_id: AccountId,
        market_id: MarketId,
        order_id: OrderId,
        nonce: u64,
    },
    DepositPerpCollateral {
        account_id: AccountId,
        market_id: MarketId,
        amount: u64,
        nonce: u64,
    },
    WithdrawPerpCollateral {
        account_id: AccountId,
        market_id: MarketId,
        amount: u64,
        nonce: u64,
    },
    SetMarginMode {
        account_id: AccountId,
        mode: crate::exchange::perp::MarginMode,
        nonce: u64,
    },
    LiquidatePerp {
        liquidator: AccountId,
        target: AccountId,
        market_id: MarketId,
        quantity: u64,
        nonce: u64,
    },
}

impl Transaction {
    /// The controller's per-account scheduler assigns the next free nonce to
    /// an otherwise fully specified manual transaction.
    pub fn with_nonce(mut self, next: u64) -> Self {
        match &mut self {
            Self::ReduceOrder { nonce, .. }
            | Self::UpdateQuotes { nonce, .. }
            | Self::ClaimAirdrop { nonce, .. }
            | Self::Transfer { nonce, .. }
            | Self::PlaceOrder { nonce, .. }
            | Self::PlacePerpOrder { nonce, .. }
            | Self::CancelOrder { nonce, .. }
            | Self::DepositPerpCollateral { nonce, .. }
            | Self::WithdrawPerpCollateral { nonce, .. }
            | Self::SetMarginMode { nonce, .. }
            | Self::LiquidatePerp { nonce, .. } => *nonce = next,
        }
        self
    }
    pub fn account_id(&self) -> AccountId {
        let account_id = match self {
            Transaction::ReduceOrder { account_id, .. } => account_id,
            Transaction::UpdateQuotes { account_id, .. } => account_id,
            Transaction::ClaimAirdrop { account_id, .. } => account_id,
            Transaction::Transfer {
                from,
                to: _,
                amount: _,
                nonce: _,
                asset: _,
            } => from,
            Transaction::PlaceOrder {
                account_id,
                market_id: _,
                order_type: _,
                side: _,
                price: _,
                quantity: _,
                nonce: _,
            } => account_id,
            Transaction::PlacePerpOrder { account_id, .. } => account_id,
            Transaction::CancelOrder {
                account_id,
                market_id: _,
                order_id: _,
                nonce: _,
            } => account_id,
            Transaction::DepositPerpCollateral {
                account_id,
                market_id: _,
                amount: _,
                nonce: _,
            } => account_id,
            Transaction::WithdrawPerpCollateral { account_id, .. } => account_id,
            Transaction::SetMarginMode { account_id, .. } => account_id,
            Transaction::LiquidatePerp { liquidator, .. } => liquidator,
        };

        *account_id
    }
    pub fn nonce(&self) -> u64 {
        let nonce = match self {
            Transaction::ReduceOrder { nonce, .. } => nonce,
            Transaction::UpdateQuotes { nonce, .. } => nonce,
            Transaction::ClaimAirdrop { nonce, .. } => nonce,
            Transaction::Transfer {
                from: _,
                to: _,
                amount: _,
                nonce,
                asset: _,
            } => nonce,
            Transaction::PlaceOrder {
                account_id: _,
                market_id: _,
                order_type: _,
                side: _,
                price: _,
                quantity: _,
                nonce,
            } => nonce,
            Transaction::PlacePerpOrder { nonce, .. } => nonce,
            Transaction::CancelOrder {
                account_id: _,
                market_id: _,
                order_id: _,
                nonce,
            } => nonce,
            Transaction::DepositPerpCollateral {
                account_id: _,
                market_id: _,
                amount: _,
                nonce,
            } => nonce,
            Transaction::WithdrawPerpCollateral { nonce, .. } => nonce,
            Transaction::SetMarginMode { nonce, .. } => nonce,
            Transaction::LiquidatePerp { nonce, .. } => nonce,
        };

        *nonce
    }
    pub fn to_digest(&self) -> Digest {
        super::digest_cache::transaction(self)
    }
    pub(super) fn digest_uncached(&self) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(b"light-dex/transaction/v4");
        match self {
            Transaction::ReduceOrder {
                account_id,
                market_id,
                order_id,
                remaining_quantity,
                nonce,
            } => {
                hasher.update(b"REDUCE_ORDER");
                hasher.update(account_id.bytes());
                hasher.update(market_id.to_le_bytes());
                hasher.update(order_id.to_le_bytes());
                hasher.update(remaining_quantity.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::UpdateQuotes {
                account_id,
                market_id,
                cancel,
                reduce,
                place,
                nonce,
            } => {
                hasher.update(b"UPDATE_QUOTES");
                hasher.update(account_id.bytes());
                hasher.update(market_id.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
                hasher.update((cancel.len() as u64).to_le_bytes());
                for id in cancel {
                    hasher.update(id.to_le_bytes());
                }
                hasher.update((reduce.len() as u64).to_le_bytes());
                for action in reduce {
                    hasher.update(action.order_id.to_le_bytes());
                    hasher.update(action.remaining_quantity.to_le_bytes());
                }
                hasher.update((place.len() as u64).to_le_bytes());
                for order in place {
                    hasher.update([match order.order_type {
                        OrderType::IOC => 0,
                        OrderType::FOK => 1,
                        OrderType::LO => 2,
                        OrderType::PostOnly => 3,
                    }]);
                    hasher.update([
                        match order.side {
                            Side::Buy => 0,
                            Side::Sell => 1,
                        },
                        u8::from(order.reduce_only),
                    ]);
                    hasher.update(order.price.to_le_bytes());
                    hasher.update(order.quantity.to_le_bytes());
                }
            }
            Transaction::ClaimAirdrop { account_id, nonce } => {
                hasher.update(b"CLAIM_AIRDROP");
                hasher.update(account_id.bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::Transfer {
                from,
                to,
                asset,
                amount,
                nonce,
            } => {
                hasher.update(b"TRANSFER");
                hasher.update(from.bytes());
                hasher.update(to.bytes());
                hasher.update(asset.to_le_bytes());
                hasher.update(amount.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::PlaceOrder {
                account_id: account,
                market_id: market,
                side,
                order_type,
                price,
                quantity,
                nonce,
            } => {
                hasher.update(b"PLACE_ORDER");

                hasher.update(account.bytes());
                hasher.update(market.to_le_bytes());

                match side {
                    Side::Buy => hasher.update([0u8]),
                    Side::Sell => hasher.update([1u8]),
                }
                match order_type {
                    OrderType::IOC => hasher.update([0u8]),
                    OrderType::FOK => hasher.update([1u8]),
                    OrderType::LO => hasher.update([2u8]),
                    OrderType::PostOnly => hasher.update([3u8]),
                }

                hasher.update(price.to_le_bytes());
                hasher.update(quantity.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::PlacePerpOrder {
                account_id,
                market_id,
                order_type,
                side,
                price,
                quantity,
                reduce_only,
                nonce,
            } => {
                hasher.update(b"PLACE_PERP_ORDER");
                hasher.update(account_id.bytes());
                hasher.update(market_id.to_le_bytes());
                hasher.update([match side {
                    Side::Buy => 0,
                    Side::Sell => 1,
                }]);
                hasher.update([match order_type {
                    OrderType::IOC => 0,
                    OrderType::FOK => 1,
                    OrderType::LO => 2,
                    OrderType::PostOnly => 3,
                }]);
                hasher.update(price.to_le_bytes());
                hasher.update(quantity.to_le_bytes());
                hasher.update([u8::from(*reduce_only)]);
                hasher.update(nonce.to_le_bytes());
            }

            Transaction::CancelOrder {
                account_id: account,
                market_id: market,
                order_id,
                nonce,
            } => {
                hasher.update(b"CANCEL_ORDER");

                hasher.update(account.bytes());
                hasher.update(market.to_le_bytes());
                hasher.update(order_id.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::DepositPerpCollateral {
                account_id,
                market_id,
                amount,
                nonce,
            } => {
                hasher.update(b"DEPOSIT_PERP_COLLATERAL");
                hasher.update(account_id.bytes());
                hasher.update(market_id.to_le_bytes());
                hasher.update(amount.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::WithdrawPerpCollateral {
                account_id,
                market_id,
                amount,
                nonce,
            } => {
                hasher.update(b"WITHDRAW_PERP_COLLATERAL");
                hasher.update(account_id.bytes());
                hasher.update(market_id.to_le_bytes());
                hasher.update(amount.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::SetMarginMode {
                account_id,
                mode,
                nonce,
            } => {
                hasher.update(b"SET_MARGIN_MODE");
                hasher.update(account_id.bytes());
                hasher.update([match mode {
                    crate::exchange::perp::MarginMode::Isolated => 0,
                    crate::exchange::perp::MarginMode::Cross => 1,
                }]);
                hasher.update(nonce.to_le_bytes());
            }
            Transaction::LiquidatePerp {
                liquidator,
                target,
                market_id,
                quantity,
                nonce,
            } => {
                hasher.update(b"LIQUIDATE_PERP");
                hasher.update(liquidator.bytes());
                hasher.update(target.bytes());
                hasher.update(market_id.to_le_bytes());
                hasher.update(quantity.to_le_bytes());
                hasher.update(nonce.to_le_bytes());
            }
        }
        hasher.finalize().into()
    }

    pub fn validate_shape(&self) -> Result<(), &'static str> {
        if let Self::UpdateQuotes {
            cancel,
            reduce,
            place,
            ..
        } = self
        {
            let count = cancel
                .len()
                .saturating_add(place.len())
                .saturating_add(reduce.len());
            if count == 0 || count > crate::exchange::limits::MAX_QUOTE_ACTIONS {
                return Err("quote batch action limit");
            }
            let unique: std::collections::HashSet<_> = cancel
                .iter()
                .copied()
                .chain(reduce.iter().map(|r| r.order_id))
                .collect();
            if unique.len() != cancel.len() + reduce.len() {
                return Err("duplicate batch order target");
            }
            if reduce.iter().any(|r| r.remaining_quantity == 0) {
                return Err("zero reduction target");
            }
        }
        Ok(())
    }

    pub fn market_id(&self) -> Option<MarketId> {
        match self {
            Self::ReduceOrder { market_id, .. }
            | Self::UpdateQuotes { market_id, .. }
            | Self::PlaceOrder { market_id, .. }
            | Self::PlacePerpOrder { market_id, .. }
            | Self::CancelOrder { market_id, .. }
            | Self::DepositPerpCollateral { market_id, .. }
            | Self::WithdrawPerpCollateral { market_id, .. }
            | Self::LiquidatePerp { market_id, .. } => Some(*market_id),
            _ => None,
        }
    }
}
