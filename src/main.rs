mod config;

use config::Config;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let config_path = args
        .windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "config/gatekeeper.toml".to_string());

    let cfg = match Config::from_file(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    println!("Gatekeeper starting");
    println!("  listen:       {}", cfg.server.listen);
    println!("  upstream_url: {}", cfg.server.upstream_url);
    println!("  algorithm:    {}", cfg.defaults.algorithm);
    println!("  capacity:     {}", cfg.defaults.capacity);
    println!("  refill_rate:  {}", cfg.defaults.refill_rate);
}
