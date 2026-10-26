use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use resolve_ingest::{
    diagnostics, discovery,
    jobs::Jobs,
    model::Cancel,
    pipeline,
    settings::{self, Destination, Settings, Verify},
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Open the desktop app on a card (to ingest) or any other folder (to convert).
    folder: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(clap::Args)]
struct IngestOptions {
    /// Preserve every regular file without requesting editing copies.
    #[arg(long)]
    archive_only: bool,
    /// Also copy clips whose verified original is already in the archive.
    #[arg(long)]
    include_archived: bool,
    /// How editing copies are checked; originals always get a full checksum readback.
    #[arg(long, value_enum, default_value_t = VerifyArg::Fast)]
    verify: VerifyArg,
}
#[derive(Clone, Copy, clap::ValueEnum)]
enum VerifyArg {
    /// Stream structure, formats, timing, and checksum.
    Fast,
    /// Also decode every audio and video frame.
    Full,
}
#[derive(Subcommand)]
enum Action {
    /// Scan and preflight. Writes nothing except temporary storage capability checks.
    Plan {
        source: PathBuf,
        archive: PathBuf,
        editing: PathBuf,
        #[command(flatten)]
        options: IngestOptions,
    },
    /// Start a new verified batch (originals are never removed).
    Ingest {
        source: PathBuf,
        archive: PathBuf,
        editing: PathBuf,
        #[command(flatten)]
        options: IngestOptions,
    },
    /// Revalidate and resume a previous batch; card is unnecessary for verified archives.
    Retry {
        batch: String,
        #[arg(long)]
        item: Option<usize>,
    },
    History,
    Report {
        batch: String,
        output: PathBuf,
    },
    /// List owned partials; --remove explicitly removes only those partials.
    Cleanup {
        batch: String,
        #[arg(long)]
        remove: bool,
    },
    /// Check installed FFmpeg and required MOV/PCM support.
    Doctor,
    /// Interactive synthetic UI slice, without source or destination writes.
    Demo,
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.command.is_none() || matches!(cli.command, Some(Action::Demo)) {
        #[cfg(feature = "desktop")]
        {
            let folder = cli.folder.map(std::fs::canonicalize).transpose()?;
            resolve_ingest::ui::run(matches!(cli.command, Some(Action::Demo)), folder);
            return Ok(());
        }
        #[cfg(not(feature = "desktop"))]
        anyhow::bail!("This build has no desktop UI. Use --help for headless commands.");
    }
    tokio::runtime::Runtime::new()?.block_on(headless(cli.command.unwrap()))
}
async fn headless(action: Action) -> Result<()> {
    let cancel = Cancel::default();
    let signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.request();
        }
    });
    let mut jobs = Jobs::open(&settings::data_dir())?;
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            if let resolve_ingest::model::Event::Message(m) = event {
                eprintln!("{m}");
            }
        }
    });
    match action {
        Action::Plan {
            source,
            archive,
            editing,
            options,
        }
        | Action::Ingest {
            source,
            archive,
            editing,
            options,
        } => {
            let is_plan = std::env::args().nth(1).as_deref() == Some("plan");
            let settings = Settings {
                source: std::fs::canonicalize(source).context("Source unavailable")?,
                archive: Some(Destination::select(archive)?),
                editing: Some(Destination::select(editing)?),
                verify: match options.verify {
                    VerifyArg::Fast => Verify::Fast,
                    VerifyArg::Full => Verify::Full,
                },
                ..Default::default()
            };
            let mut candidates = discovery::scan(&settings.source, &cancel).await?;
            if options.archive_only {
                for c in &mut candidates {
                    c.selected = true;
                    c.editing = false;
                }
            }
            if !options.include_archived {
                let archive = settings.archive.as_ref().unwrap();
                let skipped = pipeline::mark_archived(&jobs, archive, &mut candidates)?;
                if skipped > 0 {
                    eprintln!(
                        "Skipping {skipped} files already verified in this archive (--include-archived copies them again)"
                    );
                }
                if !candidates.iter().any(|c| c.selected) {
                    println!("Nothing new to ingest.");
                    return Ok(());
                }
            }
            let mut batch = pipeline::plan(settings, candidates, &cancel).await?;
            if is_plan {
                println!("{}", serde_json::to_string_pretty(&batch)?);
            } else {
                jobs.save(&batch)?;
                println!("Batch {}", batch.id);
                pipeline::run(&mut jobs, &mut batch, None, &cancel, &tx).await?;
                println!("{}: {}", batch.status, batch.summary());
                anyhow::ensure!(
                    batch.status == "Completed",
                    "Batch incomplete; use retry {}",
                    batch.id
                );
            }
        }
        Action::Retry { batch, item } => {
            let mut batch = jobs.load(&batch)?;
            if let Some(i) = item {
                anyhow::ensure!(i < batch.items.len(), "Item index out of range");
            }
            pipeline::run(&mut jobs, &mut batch, item, &cancel, &tx).await?;
            println!("{}: {}", batch.status, batch.summary());
            anyhow::ensure!(batch.status == "Completed", "Batch remains incomplete");
        }
        Action::History => {
            for batch in jobs.history()? {
                println!(
                    "{}  {}  {}  {}",
                    batch.id,
                    batch.created_at,
                    batch.status,
                    batch.summary()
                );
            }
        }
        Action::Report { batch, output } => diagnostics::export(&jobs.load(&batch)?, &output)?,
        Action::Cleanup { batch, remove } => {
            let batch = jobs.load(&batch)?;
            if remove {
                println!(
                    "Removed {} bytes of owned partials",
                    pipeline::cleanup(&jobs, &batch)?
                );
            } else {
                for (archive, path, size) in pipeline::stale_partials(&batch)? {
                    println!(
                        "{} {} {}",
                        if archive { "archive" } else { "editing" },
                        size,
                        path.display()
                    );
                }
            }
        }
        Action::Doctor => println!(
            "{}\nMOV muxer and PCM s24le encoder available",
            resolve_ingest::media::check_tools(&cancel).await?
        ),
        Action::Demo => unreachable!(),
    }
    Ok(())
}
