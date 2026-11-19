//! ida-speaker: Sprach-Lautsprecher für IDA.
//! Hört lokal auf ein Wake-Word, verbindet sich dann mit dem IDA-Voice-Dienst und führt ein Live-Gespräch –
//! mit Echo-Filter, damit Stella sich nicht selbst hört. Eingestellt wird alles über die Website (Port 8080).

mod audio;
mod cloudflare;
mod config;
mod dsp;
mod session;
mod state;
mod system;
mod wake;
mod warp;
mod web;
mod wifi;

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let data = config::data_dir();
    std::fs::create_dir_all(&data)?;
    let cfg = config::Config::load(&data.join("config.toml"))?;
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let app = state::App::new(data, cfg, cmd_tx);
    app.log(format!("ida-speaker {} startet", env!("CARGO_PKG_VERSION")));

    audio::spawn(app.clone());
    tokio::spawn(session::run(app.clone(), cmd_rx));
    tokio::spawn(warp::run(app.clone()));
    tokio::spawn(wifi::run(app.clone()));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    app.log(format!("Einstellungen unter http://<gerät>:{port}"));
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        })
        .await?;
    Ok(())
}
