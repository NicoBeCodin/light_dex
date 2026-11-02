//! Consensus byte encodings for values stored in `StateTree`.
//!
//! These are deliberately independent of Serde and `HashMap` iteration order.
//! Changing any tag, field order, integer width, or endianness changes the
//! state-root protocol; such a change requires an explicit version bump.

use crate::exchange::perp::IsolatedPerpAccount;
use crate::oracle::OraclePair;

use super::{
    OracleMark,
    account::Account,
    market::Market,
    order::{Order, Side},
};

pub(super) fn encode_oracle_mark(pair: OraclePair, mark: OracleMark) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(96);
    bytes.extend_from_slice(b"light-dex/exchange/oracle-mark/v1");
    bytes.extend_from_slice(&pair.base_asset.to_be_bytes());
    bytes.extend_from_slice(&pair.quote_asset.to_be_bytes());
    bytes.extend_from_slice(&mark.index_e8.to_be_bytes());
    bytes.extend_from_slice(&mark.update_digest);
    bytes
}

const ACCOUNT_V3: &[u8] = b"light-dex/exchange/account/v3";
const ISOLATED_PERP_ACCOUNT_V1: &[u8] = b"light-dex/exchange/isolated-perp-account/v1";
const ORDER_V2: &[u8] = b"light-dex/exchange/order/v2";
const MARKET_V3: &[u8] = b"light-dex/exchange/market/v4";
const NEXT_ORDER_ID_V1: &[u8] = b"light-dex/exchange/next-order-id/v1";

fn write_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

/// Encode every consensus-relevant account field. Asset IDs are sorted, so
/// insertion order into `balances` cannot change the resulting bytes.
pub(super) fn encode_account(account: &Account) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ACCOUNT_V3.len() + 64 + account.balances.len() * 24);
    encode_account_into(account, &mut bytes);
    bytes
}

/// The same canonical encoding with caller-owned storage. State-root batching
/// reuses a scratch buffer across accounts instead of allocating once per
/// dirty account; the protocol bytes are identical to `encode_account`.
pub(super) fn encode_account_into(account: &Account, bytes: &mut Vec<u8>) {
    bytes.clear();
    bytes.reserve(ACCOUNT_V3.len() + 64 + account.balances.len() * 24);
    bytes.extend_from_slice(ACCOUNT_V3);
    bytes.extend_from_slice(&account.id.bytes());
    write_u64(bytes, account.nonce);
    write_u64(bytes, account.used_nonces[0]);
    write_u64(bytes, account.used_nonces[1]);
    write_u64(bytes, account.balances.len() as u64);

    let mut balances: Vec<_> = account.balances.iter().collect();
    balances.sort_unstable_by_key(|&(asset_id, _)| *asset_id);
    for (&asset_id, balance) in balances {
        write_u64(bytes, asset_id);
        write_u64(bytes, balance.available);
        write_u64(bytes, balance.locked);
    }
}

fn write_i128(bytes: &mut Vec<u8>, value: i128) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

pub(super) fn encode_perp_account(account: &IsolatedPerpAccount) -> Vec<u8> {
    let position = account.position();
    let units = position.units();
    let mut bytes = Vec::with_capacity(ISOLATED_PERP_ACCOUNT_V1.len() + 91);

    bytes.extend_from_slice(ISOLATED_PERP_ACCOUNT_V1);

    write_i128(&mut bytes, account.collateral());
    write_i128(&mut bytes, position.size());
    write_i128(&mut bytes, position.open_cost_basis());
    write_i128(&mut bytes, position.realized_pnl());
    write_u64(&mut bytes, position.entry_price());

    bytes.push(units.price.decimals);
    write_u64(&mut bytes, units.price.step);
    bytes.push(units.quantity.decimals);
    write_u64(&mut bytes, units.quantity.step);
    bytes.push(units.quote_decimals);

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
    let mut bytes = Vec::with_capacity(MARKET_V3.len() + 120);
    bytes.extend_from_slice(MARKET_V3);
    write_u64(&mut bytes, market.id);
    write_u64(&mut bytes, market.base_asset);
    write_u64(&mut bytes, market.quote_asset);
    bytes.push(market.price.decimals);
    write_u64(&mut bytes, market.price.step);
    bytes.push(market.quantity.decimals);
    write_u64(&mut bytes, market.quantity.step);
    match market.market_type {
        super::market_config::MarketType::Spot => bytes.push(0),
        super::market_config::MarketType::Perp { max_leverage } => {
            bytes.push(1);
            bytes.extend_from_slice(&max_leverage.to_be_bytes());
        }
    }
    write_u64(&mut bytes, market.fees.maker_bps);
    write_u64(&mut bytes, market.fees.taker_bps);
    write_u64(&mut bytes, market.fees.liquidation_bps);
    write_u64(&mut bytes, market.fees.keeper_reward_bps);
    write_u64(&mut bytes, market.maintenance_margin_bps);
    write_u64(&mut bytes, market.funding_interval_updates);
    write_u64(&mut bytes, market.max_funding_bps);
    write_u64(&mut bytes, market.max_liquidation_slippage_bps);
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
        market_config::{DecimalStep, MarketConfig, MarketType},
    };

    #[test]
    fn account_balance_insertion_order_does_not_affect_encoding() {
        let usd = Balance {
            available: 100,
            locked: 20,
        };
        let btc = Balance {
            available: 3,
            locked: 1,
        };
        let mut first = Account {
            id: 7.into(),
            nonce: 2,
            used_nonces: [0; 2],
            balances: Default::default(),
        };
        first.balances.insert(USD, usd.clone());
        first.balances.insert(BTC, btc.clone());
        let mut second = Account {
            id: 7.into(),
            nonce: 2,
            used_nonces: [0; 2],
            balances: Default::default(),
        };
        second.balances.insert(BTC, btc);
        second.balances.insert(USD, usd);
        assert_eq!(encode_account(&first), encode_account(&second));
        let mut reused = vec![0xaa; 1024];
        encode_account_into(&first, &mut reused);
        assert_eq!(reused, encode_account(&first));
        encode_account_into(&second, &mut reused);
        assert_eq!(reused, encode_account(&second));

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
        let market = Market::from_config(
            BTC_USD,
            MarketConfig {
                base_asset: BTC,
                quote_asset: USD,
                price: DecimalStep::new(0, 1).unwrap(),
                quantity: DecimalStep::new(0, 1).unwrap(),
                market_type: MarketType::Spot,
                fees: Default::default(),
                maintenance_margin_bps: 0,
                funding_interval_updates: 0,
                max_funding_bps: 0,
                max_liquidation_slippage_bps: 0,
            },
        );
        let encoded = encode_market(&market);
        assert!(encoded.starts_with(MARKET_V3));
        assert_eq!(
            &encoded[MARKET_V3.len()..MARKET_V3.len() + 24],
            &[BTC_USD.to_be_bytes(), BTC.to_be_bytes(), USD.to_be_bytes()].concat()
        );
        let mut perp = market;
        perp.market_type = MarketType::Perp { max_leverage: 10 };
        assert_ne!(encoded, encode_market(&perp));
        assert_eq!(
            &encode_next_order_id(0x0102_0304_0506_0708)[NEXT_ORDER_ID_V1.len()..],
            &[1, 2, 3, 4, 5, 6, 7, 8]
        );
    }
}
