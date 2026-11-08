use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, time::Instant};
use voxelatlas::{
    config::{Config, Encoding},
    output, progress,
};

#[derive(Parser)]
#[command(
    version,
    about = "Minecraft Java 1.12.2 → beautiful static browser maps"
)]
struct Cli {
    /// Disable the interactive progress dashboard (logs remain available).
    #[arg(long, global = true)]
    no_progress: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Validate configuration without rendering or writing output.
    Validate {
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Render changed tiles and publish a complete static map generation.
    Render {
        #[arg(short, long)]
        config: PathBuf,
        /// Rebuild every tile regardless of incremental state.
        #[arg(long)]
        force: bool,
        /// Rehash every region payload even if filesystem metadata is unchanged.
        #[arg(long)]
        verify: bool,
        /// Print machine-readable completion statistics to stdout.
        #[arg(long)]
        json: bool,
        /// Override the number of parallel render/encode jobs.
        #[arg(long)]
        workers: Option<usize>,
        /// Override the combined decoded-chunk cache budget (not total RSS).
        #[arg(long)]
        cache_mib: Option<usize>,
        /// Encoding: fast/compact preserve pixels; indexed reduces colors and alpha.
        #[arg(long, value_enum)]
        encoding: Option<Encoding>,
    },
    /// Update player markers and inferred chunk activity independently of rendering.
    UpdateData {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long)]
        players: PathBuf,
        /// Optional UUID → {name,head} identity JSON supplied by your identity system.
        #[arg(long)]
        identities: Option<PathBuf>,
    },
    /// Remove unreachable tiles after retaining recent generations and a grace period.
    Gc {
        #[arg(short, long)]
        config: PathBuf,
        #[arg(long, default_value_t = 10)]
        keep: usize,
        #[arg(long, default_value_t = 86400)]
        grace_seconds: u64,
    },
}
fn main() {
    if let Err(error) = run() {
        eprintln!("VoxelAtlas: {error:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    progress::set_enabled(!cli.no_progress);
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "voxelatlas=info".into()),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    match cli.command {
        Command::Validate { config } => {
            let config = Config::load(&config)?;
            println!(
                "Valid configuration: {} worlds, {} maps, {} views, {} workers, {} MiB decoded-chunk cache",
                config.worlds.len(),
                config.maps.len(),
                config.maps.iter().map(|m| m.rotations.len()).sum::<usize>(),
                config.workers,
                config.cache_mib
            );
        }
        Command::Render {
            config,
            force,
            verify,
            json,
            workers,
            cache_mib,
            encoding,
        } => {
            let mut config = Config::load(&config)?;
            if let Some(workers) = workers {
                config.workers = workers;
            }
            if let Some(cache_mib) = cache_mib {
                config.cache_mib = cache_mib;
            }
            if let Some(encoding) = encoding {
                config.encoding = encoding;
            }
            config.validate()?;
            let start = Instant::now();
            let stats = output::render_with_options(&config, force, verify)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(
                        &serde_json::json!({"elapsed_seconds":start.elapsed().as_secs_f64(),"rss_mib":progress::resident_mib(),"peak_rss_mib":progress::peak_mib(),"stats":stats})
                    )?
                );
            } else {
                println!(
                    "Finished in {:.2}s · {} changed chunks · {} rendered tiles + {} zoom tiles · peak RSS {} MiB{}",
                    start.elapsed().as_secs_f64(),
                    stats.chunks_changed,
                    stats.base_tiles_rendered,
                    stats.parent_tiles_rendered,
                    progress::peak_mib(),
                    if stats.unchanged {
                        " · no output rewritten"
                    } else {
                        ""
                    }
                );
            }
        }
        Command::UpdateData {
            config,
            players,
            identities,
        } => voxelatlas::live::update(&Config::load(&config)?, &players, identities.as_deref())?,
        Command::Gc {
            config,
            keep,
            grace_seconds,
        } => {
            let config = Config::load(&config)?;
            println!(
                "Removed {} unreachable tiles",
                output::gc(&config.output, keep, grace_seconds)?
            );
        }
    }
    Ok(())
}
