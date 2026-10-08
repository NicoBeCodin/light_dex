//! Consensus byte encodings for values stored in `StateTree`.
//!
//! These are deliberately independent of Serde and `HashMap` iteration order.
//! Changing any tag, field order, integer width, or endianness changes the
//! state-root protocol; such a change requires an explicit version bump.

use super::{
    account::Account,
    market::Market,
    order::{Order, Side},
};

const ACCOUNT_V2: &[u8] = b"light-dex/exchange/account/v2";
const ORDER_V2: &[u8] = b"light-dex/exchange/order/v2";
const MARKET_V1: &[u8] = b"light-dex/exchange/market/v1";
const NEXT_ORDER_ID_V1: &[u8] = b"light-dex/exchange/next-order-id/v1";

fn write_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

/// Encode every consensus-relevant account field. Asset IDs are sorted, so
/// insertion order into `balances` cannot change the resulting bytes.
pub(super) fn encode_account(account: &Account) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ACCOUNT_V2.len() + 48 + account.balances.len() * 24);
    bytes.extend_from_slice(ACCOUNT_V2);
    bytes.extend_from_slice(&account.id.bytes());
    write_u64(&mut bytes, account.nonce);
    write_u64(&mut bytes, account.balances.len() as u64);

    let mut balances: Vec<_> = account.balances.iter().collect();
    balances.sort_unstable_by_key(|&(asset_id, _)| *asset_id);
    for (&asset_id, balance) in balances {
        write_u64(&mut bytes, asset_id);
        write_u64(&mut bytes, balance.available);
        write_u64(&mut bytes, balance.locked);
    }
    bytes
}

/// Encode one resting order. The book's FIFO priority at a price level is
/// represented by increasing order ID: IDs are assigned monotonically, and
/// `OrderBook::apply_order` only appends new resting orders to that level.
/// If the book ever permits reordering, this encoding must include priority.
pub(super) fn encode_order(order: &Order) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ORDER_V2.len() + 32 + 5 * 8 + 1);
    bytes.extend_from_slice(ORDER_V2);
    write_u64(&mut bytes, order.id);
    bytes.extend_from_slice(&order.account.bytes());
    write_u64(&mut bytes, order.market);
    bytes.push(match order.side {
        Side::Buy => 0,
        Side::Sell => 1,
    });
    write_u64(&mut bytes, order.price);
    write_u64(&mut bytes, order.original_quantity);
    write_u64(&mut bytes, order.remaining_quantity);
    bytes
}

pub(super) fn encode_market(market: &Market) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(MARKET_V1.len() + 24);
    bytes.extend_from_slice(MARKET_V1);
    write_u64(&mut bytes, market.id);
    write_u64(&mut bytes, market.base_asset);
    write_u64(&mut bytes, market.quote_asset);
    bytes
}

pub(super) fn encode_next_order_id(next_order_id: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(NEXT_ORDER_ID_V1.len() + 8);
    bytes.extend_from_slice(NEXT_ORDER_ID_V1);
    write_u64(&mut bytes, next_order_id);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::{
        account::Balance,
        market::{BTC, BTC_USD, USD},
    };
    use std::collections::HashMap;

    #[test]
    fn account_balance_insertion_order_does_not_affect_encoding() {
        let usd = Balance { available: 100, locked: 20 };
        let btc = Balance { available: 3, locked: 1 };
        let mut first = Account {
            id: 7.into(),
            nonce: 2,
            balances: HashMap::new(),
        };
        first.balances.insert(USD, usd.clone());
        first.balances.insert(BTC, btc.clone());
        let mut second = Account {
            id: 7.into(),
            nonce: 2,
            balances: HashMap::new(),
        };
        second.balances.insert(BTC, btc);
        second.balances.insert(USD, usd);
        assert_eq!(encode_account(&first), encode_account(&second));

        second.nonce += 1;
        assert_ne!(encode_account(&first), encode_account(&second));
        second.nonce -= 1;
        second.balances.get_mut(&BTC).unwrap().locked += 1;
        assert_ne!(encode_account(&first), encode_account(&second));
    }

    #[test]
    fn order_encoding_covers_execution_fields() {
        let order = Order {
            id: 5,
            account: 7.into(),
            market: BTC_USD,
            side: Side::Buy,
            price: 100,
            original_quantity: 3,
            remaining_quantity: 2,
        };
        let original = encode_order(&order);
        let mut changed = order.clone();
        changed.remaining_quantity = 1;
        assert_ne!(original, encode_order(&changed));
        changed = order.clone();
        changed.side = Side::Sell;
        assert_ne!(original, encode_order(&changed));
        changed = order.clone();
        changed.original_quantity = 4;
        assert_ne!(original, encode_order(&changed));
    }

    #[test]
    fn encodings_have_explicit_versions_and_byte_order() {
        let market = Market { id: BTC_USD, base_asset: BTC, quote_asset: USD };
        let encoded = encode_market(&market);
        assert!(encoded.starts_with(MARKET_V1));
        assert_eq!(&encoded[MARKET_V1.len()..], &[BTC_USD.to_be_bytes(), BTC.to_be_bytes(), USD.to_be_bytes()].concat());
        assert_eq!(&encode_next_order_id(0x0102_0304_0506_0708)[NEXT_ORDER_ID_V1.len()..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
