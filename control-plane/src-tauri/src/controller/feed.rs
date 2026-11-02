use super::*;

pub(super) fn feed_variance(
    history: &VecDeque<(Instant, f64)>,
    window_seconds: u64,
) -> Option<f64> {
    let now = Instant::now();
    let values = history
        .iter()
        .filter(|(at, _)| now.duration_since(*at) <= Duration::from_secs(window_seconds))
        .map(|(_, mid)| *mid)
        .collect::<Vec<_>>();
    if values.len() < 10 {
        return None;
    }
    let changes = values
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    let mean = changes.iter().sum::<f64>() / changes.len() as f64;
    Some(
        changes
            .iter()
            .map(|change| (change - mean).powi(2))
            .sum::<f64>()
            / changes.len() as f64,
    )
}

/// One Binance socket serves every fleet maker. No participant owns the feed.
pub(super) fn spawn_shared_binance_feed(shared: Shared, run_id: u64) {
    thread::spawn(move || {
        loop {
            if {
                let state = shared.lock().unwrap();
                !state.running || state.run_id != run_id
            } {
                break;
            }
            match tungstenite::connect(STREAM_URL) {
                Ok((mut socket, _)) => {
                    use tungstenite::stream::MaybeTlsStream;
                    let timeout = Some(Duration::from_secs(2));
                    match socket.get_mut() {
                        MaybeTlsStream::Plain(stream) => {
                            let _ = stream.set_read_timeout(timeout);
                        }
                        MaybeTlsStream::NativeTls(stream) => {
                            let _ = stream.get_mut().set_read_timeout(timeout);
                        }
                        _ => {}
                    }
                    loop {
                        if {
                            let state = shared.lock().unwrap();
                            !state.running || state.run_id != run_id
                        } {
                            return;
                        }
                        match socket.read() {
                            Ok(tungstenite::Message::Text(text)) => {
                                if let Ok(depth) = serde_json::from_str::<PartialDepth>(&text) {
                                    if let Some(mid) = mid_price(&depth) {
                                        let mut state = shared.lock().unwrap();
                                        if state.feed_depth.as_ref().is_none_or(|old| {
                                            depth.last_update_id >= old.last_update_id
                                        }) {
                                            let now = Instant::now();
                                            if state.feed_history.back().is_none_or(|(at, _)| {
                                                now.duration_since(*at) >= Duration::from_secs(1)
                                            }) {
                                                state.feed_history.push_back((now, mid));
                                                while state.feed_history.len() > 601 {
                                                    state.feed_history.pop_front();
                                                }
                                            }
                                            state.feed_depth = Some(depth);
                                            state.feed_at = Some(now);
                                        }
                                    }
                                }
                            }
                            Ok(tungstenite::Message::Ping(_)) => {
                                let _ = socket.flush();
                            }
                            Ok(tungstenite::Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(tungstenite::Error::Io(error))
                                if error.kind() == io::ErrorKind::WouldBlock
                                    || error.kind() == io::ErrorKind::TimedOut => {}
                            Err(error) => {
                                log(&shared, format!("Shared Binance feed lost: {error}"));
                                break;
                            }
                        }
                    }
                }
                Err(error) => log(&shared, format!("Shared Binance feed unavailable: {error}")),
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

pub(super) fn spawn_binance_feed(shared: Shared, id: u64) {
    thread::spawn(move || {
        loop {
            let active = shared
                .lock()
                .unwrap()
                .market_maker
                .as_ref()
                .is_some_and(|mm| mm.id == id && mm.active);
            if !active {
                break;
            }
            match tungstenite::connect(STREAM_URL) {
                Ok((mut socket, _)) => {
                    use tungstenite::stream::MaybeTlsStream;
                    let timeout = Some(Duration::from_secs(2));
                    match socket.get_mut() {
                        MaybeTlsStream::Plain(stream) => {
                            let _ = stream.set_read_timeout(timeout);
                        }
                        MaybeTlsStream::NativeTls(stream) => {
                            let _ = stream.get_mut().set_read_timeout(timeout);
                        }
                        _ => {}
                    }
                    loop {
                        if !shared
                            .lock()
                            .unwrap()
                            .market_maker
                            .as_ref()
                            .is_some_and(|mm| mm.id == id && mm.active)
                        {
                            return;
                        }
                        match socket.read() {
                            Ok(tungstenite::Message::Text(text)) => {
                                if let Ok(depth) = serde_json::from_str::<PartialDepth>(&text) {
                                    let mut state = shared.lock().unwrap();
                                    if let Some(mm) =
                                        state.market_maker.as_mut().filter(|mm| mm.id == id)
                                    {
                                        mm.feed_at = Some(Instant::now());
                                        if mm.latest_depth.as_ref().is_none_or(|old| {
                                            depth.last_update_id >= old.last_update_id
                                        }) {
                                            mm.latest_depth = Some(depth);
                                        }
                                    }
                                }
                            }
                            Ok(tungstenite::Message::Ping(_)) => {
                                let _ = socket.flush();
                            }
                            Ok(tungstenite::Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(tungstenite::Error::Io(error))
                                if error.kind() == io::ErrorKind::WouldBlock
                                    || error.kind() == io::ErrorKind::TimedOut => {}
                            Err(error) => {
                                report_feed_error(
                                    &shared,
                                    id,
                                    format!("Binance depth connection lost: {error}"),
                                );
                                break;
                            }
                        }
                    }
                }
                Err(error) => report_feed_error(
                    &shared,
                    id,
                    format!("Binance depth connection failed: {error}"),
                ),
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

pub(super) fn report_feed_error(shared: &Shared, id: u64, message: String) {
    let mut state = shared.lock().unwrap();
    if let Some(mm) = state.market_maker.as_mut().filter(|mm| mm.id == id) {
        mm.last_error = Some(message.clone());
    }
    state.log(message);
}
