//! Block sync responses may contain several full blocks. Serialize and write
//! them away from the consensus owner so a slow peer cannot stop voting.
use super::*;

pub(super) struct SyncWriter(mpsc::SyncSender<Job>);

struct Job {
    response: BlockSyncResponse,
    stream: TcpStream,
}

impl SyncWriter {
    pub fn new(key: [u8; 32]) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Job>(4);
        thread::Builder::new()
            .name("block-sync-writer".into())
            .spawn(move || {
                while let Ok(mut job) = receiver.recv() {
                    if let Ok(line) = crate::message::sign_compressed_line_with_limit(
                        &job.response,
                        &key,
                        72 * 1024 * 1024,
                    ) {
                        let _ = job.stream.set_write_timeout(Some(Duration::from_secs(10)));
                        let _ = job.stream.write_all(line.as_bytes());
                    }
                }
            })
            .expect("block sync writer");
        Self(sender)
    }

    pub fn send(&self, response: BlockSyncResponse, stream: TcpStream) {
        if self.0.try_send(Job { response, stream }).is_err() {
            eprintln!("block sync response queue full; requester can retry");
        }
    }
}
