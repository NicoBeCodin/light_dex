//! Bounded two-lane mailbox. Reserve capacity for consensus, but service one
//! bulk event after at most 32 priority events to avoid starving admission.
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, mpsc},
};

pub trait Prioritized {
    fn priority(&self) -> bool;
}
struct Inner<T> {
    high: VecDeque<T>,
    bulk: VecDeque<T>,
    receiver: bool,
    senders: usize,
}
struct Shared<T> {
    inner: Mutex<Inner<T>>,
    changed: Condvar,
    capacity: usize,
}
pub struct Sender<T>(Arc<Shared<T>>);
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    streak: std::cell::Cell<usize>,
}
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        inner: Mutex::new(Inner {
            high: VecDeque::new(),
            bulk: VecDeque::new(),
            receiver: true,
            senders: 1,
        }),
        changed: Condvar::new(),
        capacity,
    });
    (
        Sender(shared.clone()),
        Receiver {
            shared,
            streak: std::cell::Cell::new(0),
        },
    )
}
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.inner.lock().unwrap().senders += 1;
        Self(self.0.clone())
    }
}
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.0.inner.lock().unwrap().senders -= 1;
        self.0.changed.notify_all();
    }
}
impl<T: Prioritized> Sender<T> {
    pub fn try_send(&self, value: T) -> Result<(), mpsc::TrySendError<T>> {
        let mut inner = self.0.inner.lock().unwrap();
        if !inner.receiver {
            return Err(mpsc::TrySendError::Disconnected(value));
        }
        let queue = if value.priority() {
            &mut inner.high
        } else {
            &mut inner.bulk
        };
        if queue.len() >= self.0.capacity {
            return Err(mpsc::TrySendError::Full(value));
        }
        queue.push_back(value);
        self.0.changed.notify_all();
        Ok(())
    }
    pub fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        let mut inner = self.0.inner.lock().unwrap();
        loop {
            if !inner.receiver {
                return Err(mpsc::SendError(value));
            }
            let queue = if value.priority() {
                &mut inner.high
            } else {
                &mut inner.bulk
            };
            if queue.len() < self.0.capacity {
                queue.push_back(value);
                self.0.changed.notify_all();
                return Ok(());
            }
            inner = self.0.changed.wait(inner).unwrap();
        }
    }
}
impl<T> Receiver<T> {
    pub fn recv(&self) -> Result<T, mpsc::RecvError> {
        let mut inner = self.shared.inner.lock().unwrap();
        loop {
            let high = !inner.high.is_empty() && (self.streak.get() < 32 || inner.bulk.is_empty());
            let value = if high {
                inner.high.pop_front()
            } else {
                inner.bulk.pop_front()
            };
            if let Some(value) = value {
                self.streak.set(if high {
                    self.streak.get().saturating_add(1)
                } else {
                    0
                });
                self.shared.changed.notify_all();
                return Ok(value);
            }
            if inner.senders == 0 {
                return Err(mpsc::RecvError);
            }
            inner = self.shared.changed.wait(inner).unwrap();
        }
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.inner.lock().unwrap().receiver = false;
        self.shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    impl Prioritized for bool {
        fn priority(&self) -> bool {
            *self
        }
    }
    #[test]
    fn reserved_capacity_and_fairness() {
        let (tx, rx) = channel(40);
        for _ in 0..40 {
            tx.try_send(false).unwrap();
            tx.try_send(true).unwrap();
        }
        assert!(tx.try_send(false).is_err());
        for _ in 0..32 {
            assert!(rx.recv().unwrap());
        }
        assert!(!rx.recv().unwrap());
        drop(rx);
        assert!(tx.send(true).is_err());
    }
    #[test]
    fn disconnect_wakes_receiver() {
        let (tx, rx) = channel::<bool>(1);
        let clone = tx.clone();
        drop(tx);
        drop(clone);
        assert!(rx.recv().is_err());
    }
}
