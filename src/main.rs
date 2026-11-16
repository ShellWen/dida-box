mod config;
mod control;
mod daemon;
mod demo;
mod lighting;
mod preview;
mod spotify;
mod tray;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "Dida Box daemon and desktop controls")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Edit persistent settings and inspect the daemon
    Tui,
    /// Show desktop tray controls
    Tray,
    /// Open an independent display preview
    Preview,
    /// Save a rendered frame using the daemon's current state
    Snapshot { path: PathBuf },
    /// Print the daemon's current runtime status
    Status,
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(Command::Preview) => preview::run(None),
        Some(Command::Snapshot { path }) => preview::run(Some(path)),
        command => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            runtime.block_on(async {
                match command {
                    None => daemon::run().await,
                    Some(Command::Tui) => tui::run().await,
                    Some(Command::Tray) => tray::run().await,
                    Some(Command::Status) => {
                        println!("{}", control::Client::connect().await?.status().await?);
                        Ok(())
                    }
                    Some(Command::Preview | Command::Snapshot { .. }) => unreachable!(),
                }
            })
        }
    }
}
