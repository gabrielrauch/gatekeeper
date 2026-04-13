mod algorithm;
mod config;
mod health;
mod identity;
mod metrics;
mod middleware;
mod proxy;
mod response;
mod server;
mod store;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gatekeeper=info,tower_http=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    let config_path = args
        .windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "config/gatekeeper.toml".to_string());

    let cfg = match config::Config::from_file(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        listen = %cfg.server.listen,
        upstream_url = %cfg.server.upstream_url,
        capacity = cfg.defaults.capacity,
        refill_rate = cfg.defaults.refill_rate,
        "Gatekeeper starting"
    );

    tokio::runtime::Runtime::new()
        .expect("failed to create tokio runtime")
        .block_on(server::run(cfg));
}
