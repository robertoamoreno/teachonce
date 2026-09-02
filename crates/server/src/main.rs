//! The TeachOnce server.
//!
//! The desktop app records; this binary receives what it recorded and runs the
//! same pipeline — reconstruction, transcription, analysis, debrief, plan,
//! build — on a machine that has the model endpoint, then serves the same
//! review UI in a browser. Everything downstream of capture is the library
//! crates the app already uses, so a recording is processed identically
//! whichever side does it.
//!
//! Recordings live in the app's own folder layout under the server's data
//! directory, one folder per session. The shared crates find that directory
//! through `SKILLREC_DATA_DIR`, which is set here before anything reads it.

mod app;
mod assets;
mod auth;
mod config;
mod jobs;
mod rpc;
mod state;
mod upload;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::ServerConfig;
use crate::state::AppState;

/// Command-line options. Two flags is not worth a parser dependency.
struct Options {
    data_dir: Option<PathBuf>,
    bind: String,
    help: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut options = Self { data_dir: None, bind: "127.0.0.1:7777".into(), help: false };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--data-dir" => {
                    options.data_dir =
                        Some(PathBuf::from(args.next().context("--data-dir needs a path")?))
                }
                "--bind" => options.bind = args.next().context("--bind needs host:port")?,
                "-h" | "--help" => options.help = true,
                other => anyhow::bail!("unknown option {other}; try --help"),
            }
        }
        Ok(options)
    }
}

const USAGE: &str = "\
teachonce-server [--bind HOST:PORT] [--data-dir PATH]

  --bind       address to listen on (default 127.0.0.1:7777; use 0.0.0.0:7777
               to accept other machines on your network)
  --data-dir   where recordings, settings and built skills live
               (default: the server's application-support folder)

The API key is generated on first start, printed here, and shown in the web
UI under Settings. Clients send it as a bearer token; the browser asks for it.";

fn default_data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "teachonce", "server")
        .context("could not resolve the application data directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,skillrec=debug,teachonce_server=debug".into()),
        )
        .init();

    let options = Options::parse(std::env::args().skip(1))?;
    if options.help {
        println!("{USAGE}");
        return Ok(());
    }

    let data_dir = match options.data_dir {
        Some(dir) => dir,
        None => default_data_dir()?,
    };
    std::fs::create_dir_all(data_dir.join("sessions"))
        .with_context(|| format!("creating {}", data_dir.display()))?;
    std::fs::create_dir_all(data_dir.join("skills"))?;

    // The shared crates locate sessions, models and skills through these two
    // variables. Set before any of them is touched; this is single-threaded
    // startup, which is the one place setting process environment is sound.
    unsafe {
        std::env::set_var("SKILLREC_DATA_DIR", &data_dir);
        std::env::set_var("SKILLREC_SKILLS_DIR", data_dir.join("skills"));
    }

    let config_path = data_dir.join("server.json");
    let config = ServerConfig::load_or_create(&config_path)?;
    let state = Arc::new(AppState::new(data_dir.clone(), config_path, config));

    let listener = tokio::net::TcpListener::bind(&options.bind)
        .await
        .with_context(|| format!("binding {}", options.bind))?;
    let addr = listener.local_addr()?;
    let key = state.config.read().await.api_key.clone();
    eprintln!(
        "TeachOnce server {}\n  URL      http://{addr}\n  Data     {}\n  API key  {key}\n",
        env!("CARGO_PKG_VERSION"),
        data_dir.display()
    );

    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("shutting down");
        })
        .await
        .context("serving")?;
    Ok(())
}
