use light_dex_market_data_hub::{DEFAULT_URLS, HubHandle};
use std::{env, net::IpAddr, thread, time::Duration};

fn option(args: &[String], name: &str, fallback: &str) -> String {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| fallback.into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    let host: IpAddr = option(&args, "--listen-host", "127.0.0.1").parse()?;
    let upstream = [
        option(
            &args,
            "--binance-url",
            &env::var("LIGHT_DEX_BINANCE_WS_URL").unwrap_or_else(|_| DEFAULT_URLS[0].into()),
        ),
        option(
            &args,
            "--okx-url",
            &env::var("LIGHT_DEX_OKX_WS_URL").unwrap_or_else(|_| DEFAULT_URLS[1].into()),
        ),
        option(
            &args,
            "--coinbase-url",
            &env::var("LIGHT_DEX_COINBASE_WS_URL").unwrap_or_else(|_| DEFAULT_URLS[2].into()),
        ),
    ];
    let hub = if let Some(path) = args
        .windows(2)
        .find(|pair| pair[0] == "--feeds-file")
        .map(|p| &p[1])
    {
        let feeds = serde_json::from_slice(&std::fs::read(path)?)?;
        HubHandle::start_many(host, feeds)?
    } else {
        HubHandle::start(host, upstream)?
    };
    for venue in ["binance", "okx", "coinbase"] {
        if let Some(url) = hub.url(venue) {
            println!("{venue}: {url}");
        }
    }
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
