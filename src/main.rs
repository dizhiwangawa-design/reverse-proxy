mod api;
mod config;
mod error;
mod inband;
mod sanitize;
mod sse;
mod state;
mod store;
mod tokens;
mod upstream;

use anyhow::{Context, Result};
use config::Config;
use state::AppState;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "workbuddy_proxy=info,tower_http=warn".into()),
        )
        .with_target(false)
        .init();

    let (cfg, data_dir) = Config::load()?;
    let admin_token = cfg.admin_token(&data_dir)?;
    let state = Arc::new(AppState::new(cfg, data_dir.clone(), admin_token.clone()).await?);

    spawn_background(state.clone());

    let bind = state.config.server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let local_addr = listener.local_addr()?;

    let app = api::router(state.clone());
    let key = {
        let store = state.store.read().await;
        store
            .api_keys
            .iter()
            .find(|k| k.enabled)
            .map(|k| k.key.clone())
            .unwrap_or_default()
    };

    println!();
    println!("  WorkBuddy Proxy v{}", env!("CARGO_PKG_VERSION"));
    println!("  ------------------------------------------------------------");
    println!("  Web UI       http://{local_addr}/");
    println!("  OpenAI API   http://{local_addr}/v1");
    println!("  Anthropic    http://{local_addr}/v1/messages");
    println!("  Admin token  {admin_token}");
    if !key.is_empty() {
        println!("  API key      {key}");
    }
    println!("  Upstream     {}", state.config.upstream.endpoint);
    println!("  Data dir     {}", data_dir.display());
    println!();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    state.save_now().await;
    Ok(())
}

fn spawn_background(state: Arc<AppState>) {
    // Persist counters periodically instead of on every request.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                state.save_if_dirty().await;
            }
        });
    }

    // Keep tokens fresh and pick up rotations performed by the desktop app.
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let s = state.settings().await;
                if s.sync_local_auth {
                    state.sync_local_auth().await;
                }
                for acc in state.accounts().await {
                    let expiring = acc
                        .expires_at
                        .map(|e| e - store::now_ms() < 10 * 60 * 1000)
                        .unwrap_or(false);
                    if acc.invalid || !acc.enabled || !expiring || acc.refresh_token.is_none() {
                        continue;
                    }
                    if let Err(e) = state.refresh_account(&acc.id, false).await {
                        tracing::warn!(account = %acc.name, "background refresh failed: {}", e.message);
                    }
                }
            }
        });
    }

    // Refresh the model catalogue.
    {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            loop {
                if !state.accounts().await.is_empty() {
                    if let Err(e) = state.refresh_models().await {
                        tracing::info!("model catalogue refresh skipped: {}", e.message);
                    }
                }
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
        });
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
