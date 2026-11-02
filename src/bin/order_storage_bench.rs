//! Isolated single-price queue prototype, NOT the production book. Compare
//! persistent vector / ordered tree / linked copy-on-write pages while retaining
//! a parent snapshot. No settlement, hashes, signatures or consensus are timed.
use light_dex::exchange::order::{Order, Side};
use std::{hint::black_box, sync::Arc, time::Instant};

const PAGE: usize = 32;
#[derive(Clone)]
struct Entry {
    order: Order,
    prev: Option<u64>,
    next: Option<u64>,
}
#[derive(Clone)]
struct Page {
    entries: [Option<Entry>; PAGE],
    len: usize,
}
impl Default for Page {
    fn default() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            len: 0,
        }
    }
}
#[derive(Clone, Default)]
struct Paged {
    pages: im::OrdMap<u64, Arc<Page>>,
    ids: im::HashMap<u64, u64>,
    head: Option<u64>,
    tail: Option<u64>,
    next: u64,
}
impl Paged {
    fn entry(&self, slot: u64) -> &Entry {
        self.pages[&(slot / PAGE as u64)].entries[slot as usize % PAGE]
            .as_ref()
            .unwrap()
    }
    fn entry_mut(&mut self, slot: u64) -> &mut Entry {
        Arc::make_mut(self.pages.get_mut(&(slot / PAGE as u64)).unwrap()).entries
            [slot as usize % PAGE]
            .as_mut()
            .unwrap()
    }
    fn push(&mut self, order: Order) {
        let slot = self.next;
        self.next += 1;
        self.ids.insert(order.id, slot);
        let page = Arc::make_mut(
            self.pages
                .entry(slot / PAGE as u64)
                .or_insert_with(|| Arc::new(Page::default())),
        );
        page.entries[slot as usize % PAGE] = Some(Entry {
            order,
            prev: self.tail,
            next: None,
        });
        page.len += 1;
        if let Some(tail) = self.tail {
            self.entry_mut(tail).next = Some(slot);
        } else {
            self.head = Some(slot);
        }
        self.tail = Some(slot);
    }
    fn remove(&mut self, id: u64) -> Option<Order> {
        let slot = self.ids.remove(&id)?;
        let entry = self.entry(slot).clone();
        if let Some(prev) = entry.prev {
            self.entry_mut(prev).next = entry.next;
        } else {
            self.head = entry.next;
        }
        if let Some(next) = entry.next {
            self.entry_mut(next).prev = entry.prev;
        } else {
            self.tail = entry.prev;
        }
        let key = slot / PAGE as u64;
        let page = Arc::make_mut(self.pages.get_mut(&key).unwrap());
        page.entries[slot as usize % PAGE] = None;
        page.len -= 1;
        if page.len == 0 {
            self.pages.remove(&key);
        }
        Some(entry.order)
    }
}
fn order(id: u64) -> Order {
    Order {
        id,
        account: 1.into(),
        market: 0,
        side: Side::Sell,
        price: 100,
        original_quantity: 1,
        remaining_quantity: 1,
    }
}
fn sample(name: &str, n: u64, mut run: impl FnMut()) {
    let mut times = Vec::new();
    for i in 0..520 {
        let start = Instant::now();
        run();
        if i >= 20 {
            times.push(start.elapsed().as_nanos());
        }
    }
    times.sort_unstable();
    println!("{name},{n},{},{}", times[250], times[475]);
}
fn main() {
    println!("operation,orders,p50_ns,p95_ns");
    for n in [1000, 10000, 100000] {
        let vector: im::Vector<_> = (0..n).map(order).collect();
        let tree: im::OrdMap<u64, Order> = (0..n).map(|id| (id, order(id))).collect();
        let mut pages = Paged::default();
        for id in 0..n {
            pages.push(order(id));
        }
        sample("vector_cancel_middle", n, || {
            let mut fork = vector.clone();
            black_box(fork.remove(n as usize / 2));
        });
        sample("tree_cancel_middle", n, || {
            let mut fork = tree.clone();
            black_box(fork.remove(&(n / 2)));
        });
        sample("pages_cancel_middle", n, || {
            let mut fork = pages.clone();
            black_box(fork.remove(n / 2));
        });
        sample("vector_consume_500", n, || {
            let mut fork = vector.clone();
            for _ in 0..500 {
                black_box(fork.pop_front());
            }
        });
        sample("tree_consume_500", n, || {
            let mut fork = tree.clone();
            for id in 0..500 {
                black_box(fork.remove(&id));
            }
        });
        sample("pages_consume_500", n, || {
            let mut fork = pages.clone();
            for _ in 0..500 {
                let id = fork.entry(fork.head.unwrap()).order.id;
                black_box(fork.remove(id));
            }
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paged_random_cancels_preserve_parent_and_fifo() {
        use rand::{SeedableRng, seq::SliceRandom};
        let mut parent = Paged::default();
        for id in 0..1000 {
            parent.push(order(id));
        }
        let mut fork = parent.clone();
        let mut ids: Vec<_> = (0..1000).collect();
        ids.shuffle(&mut rand::rngs::StdRng::seed_from_u64(991));
        for &id in &ids[..500] {
            assert_eq!(fork.remove(id).unwrap().id, id);
        }
        let expected: Vec<_> = (0..1000).filter(|id| !ids[..500].contains(id)).collect();
        let mut actual = Vec::new();
        while let Some(slot) = fork.head {
            let id = fork.entry(slot).order.id;
            actual.push(fork.remove(id).unwrap().id);
        }
        assert_eq!(expected, actual);
        assert_eq!(parent.ids.len(), 1000);
        assert_eq!(parent.entry(parent.head.unwrap()).order.id, 0);
        assert!(fork.pages.is_empty());
        assert!(fork.ids.is_empty());
        assert!(fork.tail.is_none());
        fork.push(order(1001));
        assert_eq!(fork.remove(1001).unwrap().id, 1001);
    }
}
