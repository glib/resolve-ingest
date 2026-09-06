# Implementation and validation status

Implemented on 2026-09-04. This is a working initial application with a desktop executable and an independent headless core. It is **not yet a production-qualified release**: the plan's observed desktop and real NAS release gates are still open.

## Implemented

- GTK4/libadwaita dark desktop with a compact location summary, native asynchronous folder selection, a sortable virtualized footage table, filename/embedded recording dates with labeled modification-time fallback, type/date filters, bulk selection, output preview, separate archive/editing statuses, details, progress, cancellation, history, reports, output-folder actions, text scaling, and high contrast.
- Separate conversion-only workflow for existing footage: outputs beside originals or in the editing destination, preserved input filenames with an appended `.mov`, no archive dependency, input/output evidence, no-clobber publication, and journaled retry/cleanup.
- Recursive discovery that probes regular files, excludes symlinks, recognizes extensionless media, leaves probe failures visible, and previews matching-stem sidecars and explicit archive-only selections.
- Headless preflight, collision-safe reversible naming, overlap checks including resolved paths and known mount aliases, aggregate space estimates, FFmpeg checks, and per-destination publication capability checks.
- Pinned Linux directory/file descriptors, no-symlink operations, expected share identity checks, exclusive partial creation, source identity checks, SHA-256 copy/readback, sync checks, and no-clobber publication.
- Motion-video selection excluding attached pictures, all-audio PCM conversion, stream/timing/color/rotation checks, full A/V decode, and independent output SHA-256 evidence. Extra camera-data streams receive omission notices.
- A local SQLite journal with transactional per-item transitions, versioned migration, batch snapshots, startup interruption labeling, intent-before-publication, manual batch/per-file retry, archive-only-to-editing reprobe from a verified archive, and explicit owned-partial cleanup.
- Child supervision, inherited process lock, SIGTERM/escalation/wait cancellation, parent-death handling, blocked-share status, and two bounded retries for transient I/O errors.
- Three bounded processing lanes with two active assets: concurrent archive/local copies, one queued conversion/verification worker, local-input/archive checksum matching before editing publication, temporary-space reservation, journaled staging cleanup, and separate live activity lines.
- Release build, desktop entry/icon, user-prefix installation Makefile, CI, setup/storage documentation, and a reproducible real-sample harness.

## Automated checks performed

`cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`, and `cargo build --release` pass. There are 24 top-level tests: 14 core tests, six pipeline tests, one recording-metadata test, one generated-media integration workflow, and two process-supervision tests. The ignored helper test is invoked in a subprocess by the parent-death test; it is not an unexecuted acceptance test.

Conversion-only testing verifies files written beside unchanged originals, no archive/staging copies, recorded input evidence, refusal to overwrite existing outputs, rechecking a completed conversion, and rejection after source modification. Metadata testing covers both filename timestamp formats, invalid-date fallback, embedded creation times, modification-time labeling, and case-insensitive file types.

The pipeline tests hold the archive lane back and prove that two local conversions finish independently without publishing early. They check one worker per lane, the two-asset bound, journaled evidence before publication, archive failure after conversion, cancellation/draining, partial cleanup, local-input replacement rejection, and compatibility/recovery for old attempts and new staging states.

The core suite covers no-clobber conflicts, symlink escape, replaced roots, wrong share identity, overlapping locations, safe-name collisions, checksum mutation, cancellation, source mutation, recovery without the card, cleanup, writer locks, unknown DJI data codecs, and a 1,000-item journal migration/update. The generated-media workflow covers multiple AAC rates/tracks, no audio, extensionless video, sidecars, malformed media preservation, decoded frame/PCM comparison, archive-only reprobe without the source, recovery, and post-completion output corruption. The process tests verify child exit on cancellation and on forced parent termination.

## Actual Action 4 sample

Tested the user-supplied `DJI_20260826184226_0027_D.MP4`: 2,614,287,444 bytes, 227.093533 seconds, 3840×2160 HEVC Main 10 (`yuv420p10le`), 30000/1001 fps, stereo AAC at 48 kHz, BT.709 metadata, −180° rotation, timecode, DJI data tracks, and an attached JPEG. The original was read only. Test outputs were created under `target/real-action4`, using local storage.

The full ingest passed. The archive hash matches a fresh hash of the original; the 2,618,041,425-byte editing MOV passed manifest/timing/color/rotation checks, complete A/V decoding, and checksum recording. A separate proof-of-concept pass compared **all 6,806 decoded video frame hashes** and normalized source/output 24-bit PCM audio: both matched. See [machine-readable evidence](real-sample-evidence.json) and [probe/verification manifest](real-sample-manifest.json).

The same sample also passed the concurrent pipeline under `target/pipeline-action4`. Its archive and editing MOV are byte-for-byte identical to the earlier validated results. The new local-input/archive checksum check passed, and the temporary local source was removed. See [pipeline sample evidence](pipeline-sample-evidence.json). This local-storage run verifies correctness; it is not a NAS-throughput benchmark.

The Fedora `ffmpeg-free` build available in the build container lacks HEVC decoding. The real-sample test used a separate [BtbN FFmpeg 8.1 build](https://github.com/BtbN/FFmpeg-Builds/releases) unpacked under `target/tools`; it did not replace the system FFmpeg libraries. The application requires a build with the decoders for the selected camera modes. A missing decoded pixel format fails preflight with an instruction to install a suitable decoder or choose archive only.

This evidence applies to this one sample and the tested tool version. It does not establish Resolve import, observed A/V sync, HDR modes, other recording modes, or arbitrary source/output content equivalence from runtime decode checks alone.

## Desktop evidence and open release gates

The native desktop was launched under Xvfb with a software renderer. The 1,000-row synthetic prototype rendered and responded to controls; the native folder picker opened. Normal and constrained screenshots are in [design](design/). The reduced-width layout wraps status/error text and preserves visible actions. This is a smoke check, not an observed human usability or assistive-technology acceptance test. GTK portal/graphics warnings from the container do not validate native Wayland/portal integration.

A three-clip desktop ingest completed with the new pipeline. [Review](design/pipeline-review.png) shows the temporary-space allowance; [active processing](design/pipeline-active.png) shows simultaneous archive and local-copy progress with the cancel control still visible.

Still required before release:

- Actual CIFS publication, disconnect/remount, stalls, quotas/disk-full, server interruption, and directory-sync behavior on the intended NAS and Linux mount configuration.
- Actual SD-card removal during each stage; the automated source mutation/removal cases do not reproduce every card/controller failure.
- Resolve version/edition/GPU import, seeking, color, and observed A/V sync for this sample and representative high-frame-rate, split-clip, silent, and HDR/10-bit modes.
- Orca walkthrough, native Wayland folder-dialog behavior, observed 100%/150%/200% display and text scaling, reduced motion, focus/status announcements, and a user usability walkthrough. The toolkit decision remains subject to these checks.
- Longer performance and storage-fault testing of real large batches. The 1,000-row UI and journal tests do not establish throughput for 1,000 real camera clips.

The implementation conservatively rejects non-UTF8 and overlong mapped paths. Moving an unarchived source to a different mount path/identity requires rescanning into a new batch in this version. There is no cross-batch reuse, direct SMB login, automatic card deletion, split-clip joining, or alternate video-transcoding profile. ETA is withheld; stage byte/media-time progress and copy/hash speed are shown.
