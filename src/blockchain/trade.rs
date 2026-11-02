use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ts_rs::TS)]
pub struct Trade {
    pub market_id: MarketId,
    pub block_sequence: u64,
    pub transaction_index: usize,
    pub fill_index: usize,
    pub price: u64,
    pub quantity: u64,
    pub aggressor_side: Side,
    pub maker_order_id: OrderId,
    pub maker_account: AccountId,
    pub taker_order_id: OrderId,
    pub taker_account: AccountId,
    #[serde(default)]
    pub liquidation: bool,
}

impl Trade {
    pub(super) fn from_fill(
        block_sequence: u64,
        transaction_index: usize,
        fill_index: usize,
        fill: ExecutedTrade,
        liquidation: bool,
    ) -> Self {
        Self {
            market_id: fill.market_id,
            block_sequence,
            transaction_index,
            fill_index,
            price: fill.price,
            quantity: fill.quantity,
            aggressor_side: fill.aggressor_side,
            maker_order_id: fill.maker_order_id,
            maker_account: fill.maker_account,
            taker_order_id: fill.taker_order_id,
            taker_account: fill.taker_account,
            liquidation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liquidation_marker_is_derived_from_committed_transaction_kind() {
        let fill = ExecutedTrade {
            market_id: 1,
            price: 100,
            quantity: 10,
            aggressor_side: Side::Sell,
            maker_order_id: 1,
            maker_account: 1.into(),
            taker_order_id: 2,
            taker_account: 2.into(),
        };
        let ordinary = Trade::from_fill(1, 0, 0, fill.clone(), false);
        let liquidation = Trade::from_fill(1, 0, 0, fill, true);
        assert!(!ordinary.liquidation);
        assert!(liquidation.liquidation);
    }
}
