//! Run one real fixture through the public headless core without scanning its neighbors.
//! cargo run --release --example real_sample -- INPUT OUTPUT_DIRECTORY
use anyhow::{Context, Result};
use resolve_ingest::{
    jobs::Jobs,
    media,
    model::*,
    pipeline,
    settings::{Destination, Settings},
};
use std::{fs, path::PathBuf};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let input = fs::canonicalize(PathBuf::from(args.next().context("Supply input file")?))?;
    let output = PathBuf::from(args.next().context("Supply isolated output directory")?);
    fs::create_dir(&output).context("Output directory must not already exist")?;
    for dir in ["archive", "editing", "jobs"] {
        fs::create_dir(output.join(dir))?;
    }
    let cancel = Cancel::default();
    let file = fs::File::open(&input)?;
    let candidate = Candidate {
        relative: input.file_name().unwrap().into(),
        identity: Identity::of(&file.metadata()?),
        probe: Some(media::probe(&file, None, &cancel).await?),
        probe_error: None,
        selected: true,
        editing: true,
        sidecar_for: None,
        ..Default::default()
    };
    let settings = Settings {
        source: input.parent().unwrap().into(),
        archive: Some(Destination::select(output.join("archive"))?),
        editing: Some(Destination::select(output.join("editing"))?),
        ..Default::default()
    };
    let mut batch = pipeline::plan(settings, vec![candidate], &cancel).await?;
    let mut jobs = Jobs::open(&output.join("jobs"))?;
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    std::thread::spawn(move || {
        let mut stages = std::collections::BTreeMap::new();
        while let Ok(event) = rx.recv() {
            match event {
                Event::Progress { stage, .. } => {
                    let lane = Lane::for_stage(&stage);
                    if stages.get(&lane) != Some(&stage) {
                        eprintln!("{} · {stage}", lane.label());
                        stages.insert(lane, stage);
                    }
                }
                Event::Message(m) => eprintln!("{m}"),
                _ => (),
            }
        }
    });
    pipeline::run(&mut jobs, &mut batch, None, &cancel, &tx).await?;
    resolve_ingest::diagnostics::export(&batch, &output.join("report.json"))?;
    println!("{}: {}", batch.status, batch.summary());
    anyhow::ensure!(
        batch.status == "Completed",
        "Real-sample ingest incomplete; see report.json"
    );
    Ok(())
}
