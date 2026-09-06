# Resolve Ingest

A Rust desktop application for verified camera ingest on Linux. Archive untouched originals to an OS-mounted CIFS share and create MOV editing copies with unchanged video and 24-bit PCM audio.

The GTK4/libadwaita desktop remembers your three locations, scans media and sidecars, previews output paths and space, and reports original and editing status separately. SQLite records verification evidence and attempts. Recovery rechecks recorded files; verified originals can support conversion retries after the card is removed.

This is an initial implementation, not a production-qualified release. Real Action 4 modes, your Resolve installation, CIFS disconnect behavior, and observed accessibility/usability still need validation. See [implementation and validation status](docs/implementation-status.md) and the [original plan](docs/project-plan.md).

## Build and run

Requires Linux, Rust 1.88+, GTK 4.12+, libadwaita 1.5+, and FFmpeg/ffprobe with the MOV muxer and `pcm_s24le` encoder. Ubuntu 24.04+ or Fedora with those development packages can build it. The application first looks for packaged tools in `libexec/resolve-ingest` beside the installation, then uses `RESOLVE_INGEST_FFMPEG_DIR`, then falls back to `ffmpeg` and `ffprobe` on `PATH`.

```sh
# Ubuntu
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev ffmpeg
# Fedora (use a compatible FFmpeg package from your configured repositories)
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel ffmpeg-free

cargo run --release
cargo run -- doctor
cargo run -- demo       # interactive 1,000-row prototype; no media writes
make install           # installs to ~/.local, or specify PREFIX=/usr/local
# Optional: install an LGPL-only FFmpeg build with HEVC decoding alongside it
make install FFMPEG_DIR=/path/to/ffmpeg/bin
```

Choose **Archive + convert** for camera ingest, then choose a source folder and configure archive/editing destinations in Settings. **Scan folder** fills a compact table with recording date, file type, size, duration, and status. Click column headings to sort; the initial order is newest first. Filter by file type or a **Since YYYY-MM-DD** date, then use **Select shown only** to exclude older or hidden files. Filtering alone preserves your selection. **Preview output files** checks space and adds the output-filename column; **Archive & convert** starts the selected work. A remembered folder never starts work automatically. See [storage setup](docs/storage.md).

For footage already copied to disk, choose **Convert existing files**. **Alongside originals** writes `clip.mp4.mov` beside `clip.mp4`; **Use editing destination** writes under the folder configured in Settings. Sources are kept, and existing outputs are never overwritten. Scan, select videos, choose **Preview output files**, then **Convert files**. This workflow does not need a NAS or create another archive. History supports rechecking and retrying recorded conversions.

Recording dates prefer recognizable timestamps in filenames (including DJI's `YYYYMMDDHHMMSS` and `YYYYMMDD_HHMMSS`), then embedded media creation time. A `*` marks file-modification-time fallback; the date tooltip identifies its source and timezone. Filename dates use the camera clock without guessing a timezone.

Scans show the current file and number checked. **Convert existing files** skips nested Resolve-generated `CacheClip`, `ProxyMedia`, `Proxy`, `.gallery`, and `Resolve Project Backups` folders; selecting one of those folders directly still scans it. Archive scans retain all files. A media probe has a 30-second deadline; failures stay visible for retry. Cancellation and timeouts wait for child exit, which a stalled filesystem can delay.

History offers report export, recovery, partial-file inspection, and explicit cleanup. Cancel waits for active workers to stop; a stalled filesystem can delay cancellation. Verified results are never deleted by cleanup.

With a local editing destination, archiving and local copying run concurrently. Each verified local copy queues for audio conversion immediately, while other copy work continues. There is one archive worker, one local-copy worker, one conversion/verification worker, and at most two assets in flight. Editing files are published only after their input matches the verified archive. Review includes temporary-copy space, and the footer shows each active worker separately. Successful local temporary copies are removed automatically; interrupted ones remain available for History cleanup. Shared editing destinations convert from the verified archive.

## Headless workflow

```sh
cargo build --release --no-default-features
resolve-ingest plan /media/card /mnt/nas/archive /media/editing
resolve-ingest ingest /media/card /mnt/nas/archive /media/editing
resolve-ingest history
resolve-ingest retry BATCH_ID
resolve-ingest retry BATCH_ID --item 0
resolve-ingest report BATCH_ID report.json
resolve-ingest cleanup BATCH_ID          # list sizes first
resolve-ingest cleanup BATCH_ID --remove
```

`--archive-only` preserves every regular file found without requesting editing copies. `--allow-local-archive` explicitly enables local archives for tests; the desktop requires CIFS for the archive. Ctrl+C requests cancellation. Incomplete headless batches return a nonzero exit code.

Settings live in `$XDG_CONFIG_HOME/resolve-ingest/settings.toml` (default `~/.config`). The job database and application lock live in `$XDG_DATA_HOME/resolve-ingest` (default `~/.local/share`), which must be local storage. Mount credentials are not stored.

## Checks

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Media tests require FFmpeg and generate short MPEG-4/AAC fixtures, including multiple audio tracks, no audio, and an extensionless clip. They compare archived bytes, decoded video frame hashes, and normalized PCM bytes, then exercise recovery and corruption detection. Core tests exercise publication conflicts, path replacement, symlinks, locks, cancellation, and cleanup. Process tests exercise cancellation and parent-death supervision.

The supplied real Action 4 sample also passed full ingest, all 6,806 decoded video-frame comparisons, and normalized PCM comparison. [Evidence and remaining release gates](docs/implementation-status.md) distinguish this local-storage test from NAS and Resolve qualification.

To reproduce with one input file, without scanning other files in its folder:

```sh
cargo run --release --no-default-features --example real_sample -- /path/to/clip.MP4 /path/to/new-test-directory
```

Use an FFmpeg build with HEVC decoding for Action 4 HEVC files. In this workspace, the full build used for the sample test is available separately, so the desktop can be launched with it using:

```sh
PATH="$PWD/target/tools/ffmpeg-n8.1-latest-linux64-gpl-8.1/bin:$PATH" target/release/resolve-ingest
```
