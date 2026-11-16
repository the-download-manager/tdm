//! Command line front-end.

use crate::config::Config;
use crate::engine::{Engine, Outcome, Progress};
use crate::model::{Download, Header, Status};
use crate::store::Store;
use crate::util::{
    category_for, ellipsize, human_bytes, human_duration, human_speed, parse_size,
    sanitize_filename,
};
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "tdm-cli",
    version,
    about = "TDM command line - segmented download manager (same engine as the GUI)",
    long_about = "Segmented multi-connection downloads with resume, queues and a local API.\n\
                  Data lives in %APPDATA%\\tdm (override with TDM_HOME)."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Add a URL and download it now
    Add(AddArgs),
    /// List known downloads
    List,
    /// Show details of one download
    Info { id: String },
    /// Resume paused/failed downloads (all when no id is given)
    Resume { id: Option<String> },
    /// Remove a download from the list
    Remove {
        id: String,
        #[arg(long)]
        delete_file: bool,
    },
    /// Show or change configuration
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// Run the background queue plus a local dashboard/API server
    Serve {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Download an HLS (.m3u8) or DASH (.mpd) video stream
    Video(VideoArgs),
    /// Open the desktop GUI (also the default when run without arguments)
    Gui,
}

#[derive(Args, Debug)]
pub struct VideoArgs {
    /// Manifest URL (.m3u8 or .mpd)
    pub url: String,
    /// Output file (default: <download dir>/<name>.mp4)
    #[arg(short = 'o', long)]
    pub out: Option<std::path::PathBuf>,
    /// best | worst | a height such as 720
    #[arg(long, default_value = "best")]
    pub quality: String,
    /// Download only the audio track (m4a)
    #[arg(long)]
    pub audio_only: bool,
    /// List available streams instead of downloading
    #[arg(long)]
    pub list: bool,
    /// Keep the downloaded segments for debugging
    #[arg(long)]
    pub keep_temp: bool,
    /// Parallel segment downloads
    #[arg(short = 'n', long, default_value_t = 6)]
    pub connections: usize,
    /// Referer header
    #[arg(long)]
    pub referer: Option<String>,
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    Show,
    Path,
    Set { key: String, value: String },
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// URL to download
    pub url: String,
    /// Destination directory
    #[arg(short = 'd', long)]
    pub dir: Option<String>,
    /// Number of connections (segments)
    #[arg(short = 'n', long)]
    pub connections: Option<u32>,
    /// Save as this file name
    #[arg(short = 'o', long)]
    pub name: Option<String>,
    /// Extra header, e.g. -H "Authorization: Bearer x" (repeatable)
    #[arg(short = 'H', long = "header")]
    pub headers: Vec<String>,
    /// Referer header
    #[arg(long)]
    pub referer: Option<String>,
    /// Cookie header
    #[arg(long)]
    pub cookie: Option<String>,
    /// Override the User-Agent
    #[arg(long)]
    pub user_agent: Option<String>,
    /// Expected SHA-256 of the finished file
    #[arg(long)]
    pub checksum: Option<String>,
    /// Speed limit for this download, e.g. 2M
    #[arg(long)]
    pub limit: Option<String>,
    /// Do not draw a progress bar
    #[arg(long, short = 'q')]
    pub quiet: bool,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Add(args) => add(args).await,
        Command::List => list(),
        Command::Info { id } => info(&id),
        Command::Resume { id } => resume(id).await,
        Command::Remove { id, delete_file } => remove(&id, delete_file),
        Command::Config { cmd } => config_cmd(cmd),
        Command::Serve { port } => crate::server::run(port).await,
        Command::Video(args) => video(args).await,
        Command::Gui => crate::gui::run(),
    }
}

async fn video(args: VideoArgs) -> Result<()> {
    use crate::media::{self, Quality, VideoOptions};

    let cfg = Config::load();
    let quality = match args.quality.trim().to_ascii_lowercase().as_str() {
        "best" | "max" => Quality::Best,
        "worst" | "min" => Quality::Worst,
        other => match other.parse::<u32>() {
            Ok(height) => Quality::Height(height),
            Err(_) => bail!("invalid --quality '{}' (use best, worst or a height like 720)", other),
        },
    };

    let opts = VideoOptions {
        url: args.url.clone(),
        out: args.out.clone(),
        quality,
        audio_only: args.audio_only,
        list_only: args.list,
        keep_temp: args.keep_temp,
        concurrency: args.connections.max(1),
        referer: args.referer.clone(),
        progress: None,
        work_dir: None,
    };

    if args.list {
        println!("available streams in {}:", args.url);
    } else {
        println!("resolving {} ...", args.url);
    }
    let path = media::download(&opts, &cfg).await?;
    if !args.list {
        println!("saved: {}", path.display());
    }
    Ok(())
}

fn build_engine(cfg: &Arc<Config>, store: &Store) -> Result<Engine> {
    Engine::new(cfg.clone(), store.clone())
}

async fn add(args: AddArgs) -> Result<()> {
    let mut cfg = Config::load();
    if let Some(limit) = &args.limit {
        cfg.speed_limit = parse_size(limit).context("invalid --limit (try 2M)")?;
    }
    let cfg = Arc::new(cfg);
    let store = Store::new();
    store.ensure()?;

    let explicit_name = args.name.is_some();
    let file_name = match &args.name {
        Some(name) => sanitize_filename(name),
        None => sanitize_filename(
            &crate::http::filename_from_url(&args.url).unwrap_or_else(|| "download.bin".into()),
        ),
    };

    let mut dir = args
        .dir
        .clone()
        .unwrap_or_else(|| cfg.download_dir.clone());
    if args.dir.is_none() && cfg.categories {
        dir = format!("{}/{}", dir.trim_end_matches(['/', '\\']), category_for(&file_name));
    }

    let mut dl = Download::new(
        args.url.clone(),
        dir,
        file_name,
        args.connections.unwrap_or(cfg.connections),
    );
    dl.name_locked = explicit_name;
    dl.referer = args.referer.clone();
    dl.cookie = args.cookie.clone();
    dl.user_agent = args.user_agent.clone();
    dl.checksum = args.checksum.clone();
    for raw in &args.headers {
        match raw.split_once(':') {
            Some((name, value)) => {
                let name = name.trim().to_string();
                let value = value.trim().to_string();
                match name.to_ascii_lowercase().as_str() {
                    "referer" | "referrer" => dl.referer = Some(value),
                    "cookie" => dl.cookie = Some(value),
                    "user-agent" => dl.user_agent = Some(value),
                    _ => dl.headers.push(Header { name, value }),
                }
            }
            None => bail!("invalid header (expected \"Name: Value\"): {}", raw),
        }
    }

    store.save(&dl)?;
    println!(
        "added {}  {}  [{} connection{}]",
        dl.short_id(),
        ellipsize(&dl.file_name, 60),
        dl.connections,
        if dl.connections == 1 { "" } else { "s" }
    );

    let outcome = run_download(&cfg, &store, &mut dl, !args.quiet).await?;
    report_outcome(&dl, outcome);
    Ok(())
}

/// Run one download with a live progress bar and Ctrl+C -> pause.
pub async fn run_download(
    cfg: &Arc<Config>,
    store: &Store,
    dl: &mut Download,
    show_progress: bool,
) -> Result<Outcome> {
    let engine = build_engine(cfg, store)?;
    let progress = Progress::new();
    let ctl = CancellationToken::new();

    let ctrl_ctl = ctl.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            ctrl_ctl.cancel();
        }
    });

    let started = Instant::now();
    let bar = if show_progress {
        let pb = ProgressBar::new(if dl.size > 0 { dl.size } else { 0 });
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} {msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({eta})",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        pb.set_message(ellipsize(&dl.file_name, 30));
        Some(pb)
    } else {
        None
    };

    let ticker = bar.as_ref().map(|pb| {
        let pb = pb.clone();
        let progress = progress.clone();
        let stop = CancellationToken::new();
        let stop2 = stop.clone();
        let name = ellipsize(&dl.file_name, 30);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop2.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
                let total = progress.total();
                if total > 0 && pb.length() != Some(total) {
                    pb.set_length(total);
                }
                pb.set_position(progress.done());
                pb.set_message(format!(
                    "{:<30} {:>12}",
                    name,
                    human_speed(progress.speed())
                ));
            }
        });
        (stop, handle)
    });

    let outcome = engine.run(dl, ctl, progress).await;

    if let Some((stop, handle)) = ticker {
        stop.cancel();
        let _ = handle.await;
        if let Some(pb) = bar {
            pb.finish_and_clear();
        }
    }

    let elapsed = started.elapsed();
    if outcome == Outcome::Completed {
        let speed = if elapsed.as_secs_f64() > 0.0 {
            (dl.size as f64 / elapsed.as_secs_f64()) as u64
        } else {
            0
        };
        println!(
            "  {} saved in {} ({})",
            human_bytes(dl.size),
            human_duration(elapsed.as_secs().max(1)),
            human_speed(speed)
        );
    }
    Ok(outcome)
}

fn report_outcome(dl: &Download, outcome: Outcome) {
    match outcome {
        Outcome::Completed => {
            println!(
                "  {} {}",
                "completed:",
                dl.final_path().display()
            );
        }
        Outcome::Paused => {
            println!(
                "  paused at {:.1}% - continue with: tdm resume {}",
                dl.progress_fraction() * 100.0,
                dl.short_id()
            );
        }
        Outcome::Failed => {
            println!(
                "  failed: {}",
                dl.error.clone().unwrap_or_else(|| "unknown error".into())
            );
            println!("  retry with: tdm resume {}", dl.short_id());
        }
    }
}

fn list() -> Result<()> {
    let store = Store::new();
    let items = store.load_all();
    if items.is_empty() {
        println!("no downloads yet - try: tdm add <URL>");
        return Ok(());
    }
    println!(
        "{:<10} {:<11} {:>10} {:>9}  {}",
        "ID", "STATUS", "SIZE", "PROGRESS", "NAME"
    );
    println!("{}", "-".repeat(78));
    for dl in items {
        let progress = if dl.status == Status::Completed {
            "100.0%".to_string()
        } else {
            format!("{:.1}%", dl.progress_fraction() * 100.0)
        };
        println!(
            "{:<10} {:<11} {:>10} {:>9}  {}",
            dl.short_id(),
            dl.status.as_str(),
            human_bytes(dl.size),
            progress,
            ellipsize(&dl.file_name, 36)
        );
    }
    Ok(())
}

fn info(id: &str) -> Result<()> {
    let store = Store::new();
    let dl = store
        .resolve(id)
        .with_context(|| format!("no download matching '{}'", id))?;
    println!("id          : {}", dl.id);
    println!("url         : {}", dl.url);
    if let Some(final_url) = &dl.final_url {
        if final_url != &dl.url {
            println!("final url   : {}", final_url);
        }
    }
    println!("file        : {}", dl.final_path().display());
    println!("status      : {}", dl.status.as_str());
    println!(
        "size        : {} ({} bytes)",
        human_bytes(dl.size),
        dl.size
    );
    println!(
        "downloaded  : {} ({:.1}%)",
        human_bytes(dl.downloaded),
        dl.progress_fraction() * 100.0
    );
    println!("connections : {}", dl.connections);
    if !dl.segments.is_empty() {
        for (i, seg) in dl.segments.iter().enumerate() {
            println!(
                "  #{} bytes {}-{}  {:.1}%",
                i + 1,
                seg.start,
                seg.end.saturating_sub(1),
                if seg.len() > 0 {
                    seg.done as f64 / seg.len() as f64 * 100.0
                } else {
                    0.0
                }
            );
        }
    }
    if let Some(etag) = &dl.etag {
        println!("etag        : {}", etag);
    }
    if let Some(last_modified) = &dl.last_modified {
        println!("last-modified: {}", last_modified);
    }
    println!("category    : {}", dl.category);
    if let Some(error) = &dl.error {
        println!("error       : {}", error);
    }
    Ok(())
}

async fn resume(id: Option<String>) -> Result<()> {
    let cfg = Arc::new(Config::load());
    let store = Store::new();

    let targets: Vec<Download> = match id {
        Some(id) => vec![store
            .resolve(&id)
            .with_context(|| format!("no download matching '{}'", id))?],
        None => store
            .load_all()
            .into_iter()
            // a "downloading" record with no process behind it is a crashed run: resumable
            .filter(|d| {
                matches!(
                    d.status,
                    Status::Paused | Status::Error | Status::Queued | Status::Downloading | Status::Connecting
                )
            })
            .collect(),
    };

    if targets.is_empty() {
        println!("nothing to resume");
        return Ok(());
    }

    for mut dl in targets {
        if dl.status == Status::Completed {
            println!("{} already completed", dl.short_id());
            continue;
        }
        println!("resuming {}  {}", dl.short_id(), ellipsize(&dl.file_name, 50));
        let outcome = run_download(&cfg, &store, &mut dl, true).await?;
        report_outcome(&dl, outcome);
    }
    Ok(())
}

fn remove(id: &str, delete_file: bool) -> Result<()> {
    let store = Store::new();
    let dl = store
        .resolve(id)
        .with_context(|| format!("no download matching '{}'", id))?;
    if delete_file {
        let final_path = dl.final_path();
        let part_path = dl.part_path();
        for path in [final_path, part_path] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("deleting {}", path.display()))?;
                println!("deleted {}", path.display());
            }
        }
    }
    store.delete(&dl.id)?;
    println!("removed {}  {}", dl.short_id(), ellipsize(&dl.file_name, 50));
    Ok(())
}

fn config_cmd(cmd: Option<ConfigCmd>) -> Result<()> {
    match cmd {
        None | Some(ConfigCmd::Show) => {
            let cfg = Config::load();
            println!("# {}", Config::path().display());
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        Some(ConfigCmd::Path) => println!("{}", Config::path().display()),
        Some(ConfigCmd::Set { key, value }) => {
            let mut cfg = Config::load();
            cfg.set(&key, &value)?;
            cfg.save()?;
            println!("{} = {}", key, value);
        }
    }
    Ok(())
}
