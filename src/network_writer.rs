//! Bounded FIFO per peer. A slow peer can stall only its own writer, not the
//! validator owner. FIFO deliberately preserves consensus-message ordering.
use std::{
    io::{self, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct PeerTransportStats {
    pub write_timeouts: AtomicU64,
    pub write_errors: AtomicU64,
    pub queue_full: AtomicU64,
    pub truncated_frames: AtomicU64,
    pub reconnects: AtomicU64,
}

pub struct PeerWriter {
    tx: mpsc::SyncSender<(Arc<[u8]>, Option<Instant>)>,
    queued_bytes: Arc<AtomicUsize>,
    alive: Arc<AtomicBool>,
    socket: TcpStream,
    stats: Arc<PeerTransportStats>,
    trace: bool,
}
impl PeerWriter {
    pub fn new(socket: TcpStream) -> io::Result<Self> {
        Self::with_stats(socket, Arc::new(PeerTransportStats::default()))
    }
    pub fn with_stats(mut socket: TcpStream, stats: Arc<PeerTransportStats>) -> io::Result<Self> {
        socket.set_nodelay(true)?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        let shutdown = socket.try_clone()?;
        let (tx, rx) = mpsc::sync_channel::<(Arc<[u8]>, Option<Instant>)>(512);
        let trace = std::env::var_os("LIGHT_DEX_HANDOFF_TRACE").is_some();
        let peer = socket.peer_addr().ok();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let bytes = queued_bytes.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let running = alive.clone();
        let writer_stats = stats.clone();
        std::thread::Builder::new()
            .name("peer-writer".into())
            .spawn(move || {
                while let Ok((payload, enqueued)) = rx.recv() {
                    let queued_us = enqueued.map(|at| at.elapsed().as_micros());
                    let write_started = trace.then(Instant::now);
                    let result = socket.write_all(&payload);
                    if trace && payload.len() >= 8 * 1024 {
                        eprintln!("HANDOFF event=peer_write peer={peer:?} bytes={} queued_us={} write_us={} ok={}",
                            payload.len(), queued_us.unwrap_or(0),
                            write_started.map(|at| at.elapsed().as_micros()).unwrap_or(0), result.is_ok());
                    }
                    bytes.fetch_sub(payload.len(), Ordering::Relaxed);
                    if let Err(error) = result {
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) {
                            writer_stats.write_timeouts.fetch_add(1, Ordering::Relaxed);
                        }
                        writer_stats.write_errors.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
                running.store(false, Ordering::Release);
                let _ = socket.shutdown(Shutdown::Both);
            })?;
        Ok(Self {
            tx,
            queued_bytes,
            alive,
            socket: shutdown,
            stats,
            trace,
        })
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Relaxed)
    }
    pub fn send(&self, payload: Arc<[u8]>) -> io::Result<()> {
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        if !self.is_alive() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let len = payload.len();
        if self
            .queued_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |size| {
                size.checked_add(len).filter(|next| *next <= MAX_BYTES)
            })
            .is_err()
        {
            self.stats.queue_full.fetch_add(1, Ordering::Relaxed);
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.tx
            .try_send((payload, self.trace.then(Instant::now)))
            .map_err(|error| {
                self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                match error {
                    mpsc::TrySendError::Full(_) => {
                        self.stats.queue_full.fetch_add(1, Ordering::Relaxed);
                        io::ErrorKind::WouldBlock.into()
                    }
                    mpsc::TrySendError::Disconnected(_) => io::ErrorKind::BrokenPipe.into(),
                }
            })
    }
}
impl Drop for PeerWriter {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn writer_preserves_fifo_and_rejects_oversize_without_consuming_budget() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let writer = PeerWriter::new(client).unwrap();
        assert_eq!(
            writer
                .send(vec![0u8; 8 * 1024 * 1024 + 1].into())
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        writer.send(Arc::from(&b"first\n"[..])).unwrap();
        writer.send(Arc::from(&b"second\n"[..])).unwrap();
        let mut output = [0u8; 13];
        server.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"first\nsecond\n");
    }
}
