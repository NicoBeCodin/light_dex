//! Same-host timing registration for the independently supervised demo fleet.
//! Only digests cross this private socket. No public endpoint, keys, disk writes,
//! or additional validator reads are needed. Commit observation stays in observer.
use super::*;

fn register(pending: &Mutex<HashMap<String, Instant>>, digest: &str) {
    if hex_decode::<32>(digest).is_err() {
        return;
    }
    let mut pending = pending.lock().unwrap();
    if pending.len() < 10_000 {
        pending
            .entry(digest.to_owned())
            .or_insert_with(Instant::now);
    }
}

#[cfg(unix)]
pub(super) fn start(app: Shared) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let path = app.config.database.with_extension("timing.sock");
    // Called only after taking the gateway TCP listener, so a second gateway
    // cannot unlink the active process's socket. Never unlink unrelated files.
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(&path)?,
        Ok(_) => return Err(std::io::Error::other("timing path is not a socket")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let socket = tokio::net::UnixDatagram::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tokio::spawn(async move {
        let mut bytes = [0u8; 65];
        while let Ok(length) = socket.recv(&mut bytes).await {
            if length == 64 {
                if let Ok(digest) = std::str::from_utf8(&bytes[..length]) {
                    register(&app.pending, digest);
                }
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn start(_app: Shared) -> std::io::Result<()> {
    Ok(())
}

pub(super) fn announce(database: &std::path::Path, digest: &str) {
    #[cfg(unix)]
    if let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() {
        let _ = socket.set_nonblocking(true);
        // Best-effort telemetry must never slow down or block agent admission.
        let _ = socket.send_to(digest.as_bytes(), database.with_extension("timing.sock"));
    }
    #[cfg(not(unix))]
    let _ = (database, digest);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_preserve_the_first_submission_clock_and_malformed_digests_are_ignored() {
        let pending = Mutex::new(HashMap::new());
        let digest = "ab".repeat(32);
        register(&pending, &digest);
        let first = pending.lock().unwrap()[&digest];
        register(&pending, &digest);
        register(&pending, "invalid");
        assert_eq!(pending.lock().unwrap()[&digest], first);
        assert_eq!(pending.lock().unwrap().len(), 1);
    }

    #[test]
    fn pending_measurements_are_bounded() {
        let pending = Mutex::new(
            (0..10_000)
                .map(|n| (format!("{n:064x}"), Instant::now()))
                .collect(),
        );
        register(&pending, &"ff".repeat(32));
        assert_eq!(pending.lock().unwrap().len(), 10_000);
    }
}
