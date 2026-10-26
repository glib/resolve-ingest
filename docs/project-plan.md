# Resolve Ingest project plan

> **Revisions (2026-10-25).** After using the first implementation, these decisions changed in favour of a one-click card workflow: archives may be on any kernel-mounted local, removable, CIFS, or NFS filesystem (GVfs/FUSE is still refused); originals use a readable `<recording day>/<filename>` layout, and clips already verified in the archive are skipped on later ingests; the full decode of editing copies is optional, with structure/timing/checksum checks by default; cards are detected automatically and preflight runs in the background, without a separate preview step. See [validation status](validation/status.md) and [storage](storage.md). The rest of this document is the original plan.

## Scope and defaults

Build a Linux-first Rust GUI for a repeatable SD-card ingest workflow. Remember three independent folder selections: source card, editing output, and original archive on a mounted Samba/CIFS share. The editing destination may also be on a share. Selecting a remembered path never starts an ingest automatically.

Preserve originals byte for byte; never modify, delete, or format the card. Copy video into MOV editing files without re-encoding it, and convert audio to 24-bit little-endian PCM while retaining sample rate and channel layout. PCM conversion does not restore information lost to AAC compression.

For the first release, the operating system handles mounting and authenticating the NAS. Support OS-mounted CIFS paths; desktop `smb://` URLs and GVfs/FUSE mounts are outside the initial supported storage profile. Explain how to select a supported mount. Remember the expected share identity as well as its path so an unmounted share cannot silently become a local archive folder. Apply the same safeguards to a shared editing destination. Direct SMB connections and credential storage are outside the initial scope.

## User flow

1. Show the remembered source and editing destination, plus archive location and availability. Put archive setup and conversion preferences in Settings, with a visible archive summary on the main screen.
2. Scan the selected source recursively. Probe candidate videos rather than trusting extensions alone. Present clip count, size, duration, and selection checkboxes. Keep failed probes visible with Retry and Archive only actions; a failed probe must not prevent preservation of a selected original. Exclude ordinary nonmedia files by default, but allow explicit archive-only selection. Show sidecars separately; default to same-directory, matching-stem associations confirmed against real card fixtures, with the selection preview authoritative. Archive selected sidecars without converting them. Preserve camera split clips independently.
3. Preflight paths, mount identity, read/write access, available space, and FFmpeg tools for requested conversions. Reject overlapping source/output/archive trees, including resolved symlinks and known mount aliases. Do not follow source symlinks or ingest nonregular files. Check no-clobber publication support before starting; unsupported storage gets an actionable error. Handle unavailable card paths by asking the user to select the current mount. Show planned output paths and estimated space before Start.
4. Start a durable batch. Show each file's archive and editing status independently, plus overall counts, bytes, current stage, speed, and estimated remaining time when reliable.
5. Allow Cancel, Retry failed, and per-file Retry. Cancellation stops scheduling and terminates active conversion cleanly; partial files remain explicitly incomplete. A blocked share pauses affected work and offers Retry after reconnection. Completed originals remain useful even if conversion fails.
6. Finish with verified/completed, failed, skipped, and canceled counts, separating editing clips from archive-only files, plus links to output folders and an exportable report. A failed sidecar makes the batch incomplete but does not block editing a successfully archived video. On restart, offer recovery of interrupted batches. Provide explicit cleanup of owned stale partials with their sizes, without removing verified results.

Suggested layout: source and destination selectors at the top, archive status underneath, a clip table in the center, and overall progress and actions at the bottom. Error details expand from a row and include the failed operation and a concrete next step.

## UI/UX design and delivery

Deliver a clean, modern, professional desktop application with a polished dark theme as the default. Visual quality, clear feedback, and accessibility are release requirements, with design work starting before the full GUI implementation. The following choices are project design decisions informed by the referenced tools and guidelines; they are not claims that those tools implement this exact workflow.

### Research and interaction direction

HandBrake separates preparation from queued execution and makes pending jobs inspectable. Adopt a clear selection/review step and a persistent job list, while keeping this application's fixed ingest profile simple. See [HandBrake's queue documentation](https://handbrake.fr/docs/en/latest/advanced/queue.html).

OffShoot documents distinct verification methods and verification logs. Adopt clear verification labels and inspectable evidence in row details and batch reports; retain this project's full archive checksum readback policy. See [OffShoot's verification documentation](https://docs.hedge.video/offshoot/features/verification).

Use GNOME's guidance on [dark styling](https://developer.gnome.org/hig/guidelines/ui-styling.html), [accessibility](https://developer.gnome.org/hig/guidelines/accessibility.html), and [persistent banners](https://developer.gnome.org/hig/patterns/feedback/banners.html) for desktop behavior. Our dark default suits the requested media workflow. Pair status color with words/icons, support keyboard and assistive technology, and keep actionable failures visible until resolved.

### Screen structure

- **Ingest:** the main working screen, with three clearly labeled locations: Source card, Original archive, and Editing destination. Show readable folder names, availability, and a full-path reveal/copy action. Keep archive configuration in Settings but its active location visible here.
- **Clip list:** stable rows with selection, filename, duration/size, Original archive status, and Editing copy status. Expand a row for metadata, sidecars, full paths, verification evidence, and errors. Show All / Needs attention / Completed filters; preserve selection and scroll position during updates. Defer thumbnails unless user testing shows they materially improve selection.
- **Activity footer:** persistent current-stage progress, batch completion counts, and one prominent contextual action: Start ingest before work, Retry failed after errors. Cancel remains a secondary action during work. Show the reason beside disabled Start, rather than requiring a tooltip to discover it.
- **History and Settings:** secondary views for previous batches/reports and remembered locations/preferences. Keep technical logs and advanced details behind explicit disclosure.

At normal desktop width, align the location rows above the clip list and keep status columns easy to scan. At smaller widths, stack location controls and move secondary metadata into row details. Long paths must never push actions offscreen. Snapshot settings into each batch; changing preferences applies to the next batch, never silently redirects running work.

### Visual system

Use charcoal backgrounds with slightly lighter panels, restrained borders, and one cool accent for primary actions and focus. Initial token candidates: background `#15181D`, surface `#1E232B`, raised surface `#272E38`, primary text `#EDF0F5`, secondary text `#AEB8C5`, accent `#7AA2F7`; validate actual foreground/background combinations before freezing them. Reserve green, amber, and red for semantic status, always paired with labels and icons.

Use a single readable sans-serif family, 14–16 logical-pixel body text, restrained heading sizes, and tabular numerals for progress values. Adopt a 4/8-pixel spacing scale, 8-pixel control corners, consistent 36–40-pixel controls, and one coherent vector icon set. Use whitespace and alignment to establish hierarchy; avoid decorative gradients, glass effects, excessive cards, or attention-grabbing animation. Define hover, pressed, selected, disabled, focus, error, and loading states as reusable components rather than per-screen styling.

### Feedback and recovery language

Show explicit stages such as Copying original, Checking original, Converting audio, and Checking editing copy. Report byte progress for copy/hash passes and media-time progress for conversion/decode; use an indeterminate indicator when totals are unavailable. Never imply an entire clip is complete because its copy reached 100%. The batch footer uses completed counts; any estimated time includes remaining verification and is withheld when unreliable.

Show errors in the affected row and a persistent batch banner, for example: “Archive unavailable. Reconnect the share, then retry. 8 originals are already verified.” Keep detailed tool output behind Details / Copy diagnostics. Use brief toasts only for noncritical confirmations. Avoid repeated modal failures; use a dialog when a decision is required, including closing with active work. Closing offers Keep ingesting or Cancel and close; it does not silently abandon a job or imply background operation. Cancellation pending remains visible until workers stop.

Design empty, scanning, ready, running, blocked, partially successful, canceled, recovered, and completed states explicitly. Completion distinguishes archive-only files from editable clips and provides Open editing folder, View report, and Retry failed where applicable. Desktop completion/error notifications are optional and must not replace persistent in-app state.

### Accessibility and design acceptance

Provide complete keyboard operation, a visible focus ring, meaningful accessible names, logical focus order, and status announcements that do not read every progress tick. Target at least 4.5:1 contrast for normal text and 3:1 for essential control boundaries/focus indicators as project acceptance thresholds. Support text scaling to 200%, high-contrast presentation, reduced motion, and 100%/150%/200% display scaling without clipped actions. Test the primary workflow with a screen reader on the supported Linux desktop.

Before committing to the GUI toolkit, build a small interactive slice covering a location picker, clip row, progress, error/retry, and keyboard/screen-reader navigation. Iced is provisional: if the tested release cannot meet these requirements, evaluate Rust GTK4/libadwaita before building the full UI. Record the choice and evidence, including Linux folder-dialog behavior.

Produce high-fidelity dark-theme mockups and an interactive prototype for the key states during milestone 1. Validate the normal and constrained desktop layouts, long filenames, many clips, and partial failure. Review screenshots/component states during implementation. Release acceptance requires a short observed usability walkthrough: a user can identify where originals and editing copies go, start a repeat ingest without reconfiguration, distinguish archived from ready to edit, and recover a failed item without opening logs. A representative 1,000-row synthetic batch must stay responsive without progress updates stealing focus or scrolling the list.

## Technical approach

- **GUI:** provisionally Iced, using its task/subscription model to receive worker events without blocking rendering, subject to the UI/accessibility prototype gate above. Native folder selection through a suitable Rust dialog library, validated on the target Linux desktop.
- **Workers:** Tokio process orchestration and bounded channels; blocking filesystem and hashing work runs off the UI executor. Following the pipelining request, use one archive worker, one local-copy worker, and one conversion/verification worker, with at most two active assets. Local editing copies can start conversion before archive verification, while publication still waits for the verified archive and matching input checksum. Cancellation waits for all workers and child exit before retry or cleanup. Use child supervision and unique attempt paths to prevent orphaned FFmpeg processes after an application crash from racing recovery; resolve ownership before touching abandoned attempts.
- **Media:** installed `ffmpeg` and `ffprobe` subprocesses, with version and required encoder/muxer checks at startup. Pass argument arrays directly; never build shell commands from filenames.
- **Preferences:** versioned TOML in the XDG configuration directory, saved through a temporary file and atomic replacement. Store paths and preferences, not credentials.
- **Jobs:** SQLite in the local XDG data directory, never on the SMB share. Use transactions for state transitions and a single application/job lock to prevent duplicate writers.
- **Diagnostics:** structured local logs, bounded FFmpeg stderr capture, and user-exported batch manifests. Reports contain paths but never mount credentials.

Proposed modules: `ui`, `settings`, `discovery`, `media`, `pipeline`, `storage`, `verification`, `jobs`, and `diagnostics`. Keep the pipeline independent of Iced so it can be exercised without a display.

## File pipeline and durable state

Track archive and conversion stages separately for each item. Discovery/probing determines editing eligibility, independently of archive eligibility:

`selected → copying → verifying_archive → archived`

For conversion-eligible clips on local editing storage, run `copying_locally → verifying_local_copy → queued_conversion → converting → verifying_output → awaiting_archive → complete` alongside archival. Shared editing destinations queue conversion after archive verification. Archive-only items complete at `archived`; record conversion as not requested, not successful. Probe failures remain visible and may be retried against the verified archive later.

Each stage may become failed, canceled, or interrupted. Record the last successful stage, attempt count, error category, source identity, relative path, source size/mtime/hash, target paths, probe results, conversion profile, tool version, output hash, and verification results. Recovery rechecks files; database state alone is not proof of completion.

1. Allocate a unique batch ID and stable paths, such as `archive/2026-09-04/<batch-id>/originals/DCIM/...` and `editing/2026-09-04/<batch-id>/DCIM/.../<stem>.mov`. Persist those paths before writing. Resolve same-stem collisions deterministically and never overwrite an unrelated existing file. Account for destination case folding, Unicode/name restrictions, path lengths, and file/directory collisions. Use a reversible safe name mapping recorded in the manifest, or fail preflight with the affected paths.
2. Stream-copy the source to a uniquely owned temporary file beside its final archive path, computing SHA-256 as bytes are read. Check source identity, size, and timestamps before and after; if they change, fail and rescan.
3. Flush/sync and close the archive temporary file, then reopen and hash it. Require size and hash equality with the source stream. Persist the expected hash and publication intent before publishing. Publish using a same-filesystem, no-clobber rename mechanism validated for the target mount; sync the containing directory where supported and check all sync errors before recording archive success. Record unsupported directory-sync semantics as a storage capability, not a promise of durability. Do not rely on an existence check followed by an overwriting rename.
4. For a local editing destination, independently copy the card to a journaled temporary source while archiving proceeds. Check identity and checksum readback before queuing conversion, and reserve temporary capacity for at most two originals. Convert from this local source without waiting for the archive; require its checksum to match the verified archive before publishing the editing output. Conversion retries can stage from a reverified archive without the card. Shared editing destinations convert directly from the verified archive. Write a temporary MOV beside the final editing output. Preserve all selected audio streams and the primary video; report additional data streams that cannot be preserved in MOV. Originals retain all camera data.
5. After FFmpeg exits successfully, flush/sync and close the converted temporary file, then reopen it for structural, full-decode, and checksum verification. Persist its checksum, validation evidence, and publication intent before publishing without overwriting an existing file. Apply the same directory-sync and recovery rules as archival. A requested editing clip becomes complete only after both archive and output verification pass. Record current availability separately from verification at the recorded time.

On restart, reconcile temporary and final files with the journal, including crashes between rename and database update. Reuse verified archives; restart an interrupted copy or conversion from the beginning in v1. Skip an existing result only when content identity, conversion profile, and verification evidence match; filenames and timestamps alone are insufficient. Revalidate before reuse when recovering interrupted work. Retry creates new attempt records without discarding earlier diagnostics.

Only clean up temporary files owned by the current job. Failed verification must never produce a final successful filename. Distinguish matching duplicates from conflicts in the UI. Within a batch, retry reuses verified stages. Across new batches, v1 defaults to creating a new archive/output set; content-based cross-batch reuse is deferred and must not be implied by a filename-based skip.

## Conversion and verification

Illustrative conversion for a clip with one primary video and optional audio:

```sh
ffmpeg -nostdin -hide_banner -n -xerror -err_detect explode -i INPUT.mp4 \
  -map 0:v:0 -map '0:a?' -map_metadata 0 -map_chapters 0 \
  -c:v copy -c:a pcm_s24le \
  -progress pipe:1 -nostats -f mov OUTPUT.partial
```

The application supplies these as separate arguments and drains stdout and stderr concurrently. The example assumes the primary video is index `0:v:0`; production selects the probed motion-video stream explicitly, excluding attached pictures. Parse FFmpeg's machine-readable progress against probed duration. A zero exit status and `progress=end` are necessary but insufficient for success. Clips without audio are explicitly labeled and remuxed without inventing an audio track. Unsupported source streams receive a clear error or an explicit omission notice. Compare stream-level rotation, timecode, and color metadata and record omissions; metadata mapping does not promise preservation of arbitrary camera metadata.

Build an expected output stream manifest from the source probe and conversion profile; compare against this manifest, not raw input stream counts when data streams are intentionally omitted. Use `ffprobe` JSON to compare video codec, dimensions, pixel format, frame rate, color metadata, expected stream counts, audio codec, sample rate, channel layout, durations, and timestamp offsets. Define timing tolerances from frame/sample timing and AAC priming using real fixtures; do not require naïve exact duration equality. Reject empty or unexpectedly truncated streams. Strict conversion error handling prevents detected AAC decode failures from silently producing apparently valid PCM; capture warnings and establish their treatment using fixtures.

By default, perform a full output decode with FFmpeg error detection (for example, `-v error -xerror -err_detect explode -i OUTPUT -map 0:v -map '0:a?' -f null -`) after structural checks. Map every expected A/V stream explicitly; FFmpeg's automatic stream selection is insufficient for multitrack verification. Capture exit status and diagnostics. This adds a full read/decode pass, so show verification as a distinct progress stage and include it in estimates. Persist an output SHA-256 for later corruption checks against that independent baseline. During the media proof of concept, compare decoded video frame hashes separately from timestamp checks. Compare source-decoded PCM against output PCM after normalizing both to s24le, identical sample rate/channel layout, and the same trimming/priming policy. These checks establish preservation for the tested profile and fixtures. Runtime probe/decode checks establish structural validity and decodability, not general proof of source/output content equivalence; label the verification evidence accordingly.

Archive checksums establish equality of bytes read back through the client filesystem. They cannot certify NAS disk redundancy or server-side durable storage; report the concrete verification performed.

## Space, errors, and recovery

Estimate archive size from source bytes. Estimate output from copied video plus the sum of `duration × sample_rate × channels × 3` bytes of PCM for every selected audio stream, plus container overhead and a safety margin. Aggregate requirements when outputs share a filesystem, account for existing verified data and temporary files, and handle quota/space failures even after preflight.

Check mount identity for every shared destination before operations and publication; treat disconnects or mount changes as blocked work. Store stable expected share identity separately from the current mount-instance identity; accept a legitimate remount only after validating the expected share and reopening roots. Resolve validated roots to pinned directory handles and use relative, no-symlink filesystem operations so path replacement cannot redirect an in-flight write to a local fallback directory. Use securely created, exclusively owned temporary files. Validate how FFmpeg receives pinned input/output targets in the proof of concept; plain path checks followed by path-based writes are insufficient. The invariant is that opened objects belong to validated roots without symlink escape or local fallback; choose the platform-specific mechanism after testing actual CIFS storage. Fail closed if the supported mount cannot satisfy the storage contract. Filesystem operations on an unresponsive CIFS mount can stall despite cancellation; keep the GUI responsive, show cancellation pending, and do not start a competing retry until the previous worker has stopped. Test mount timeout behavior rather than promising instant cancellation.

Classify errors: source removed/changed, destination unavailable, permission denied, insufficient space, existing-file conflict, checksum mismatch, malformed media, missing codec/tool, conversion failure, and verification failure. Retry transient failures with a small bounded backoff; do not repeatedly retry bad media or permissions automatically. Manual retry resumes from the earliest unverified stage. Preserve verified work when another item fails.

## Implementation milestones

1. **Media, filesystem, and UI proof of concept.** Collect short real Action 4 samples covering actual camera modes, including 10-bit/HDR if used, high frame rate, split clips, and no audio. Confirm Resolve version/edition/GPU, import PCM MOVs, check A/V sync, color, metadata, and seeking. Validate NAS mount detection, readback hashing, publication semantics, and interruption behavior. Develop the dark-theme mockups and interactive toolkit/accessibility slice described above. Exit: a proven conversion profile, storage contract, chosen GUI toolkit, reviewed visual direction, and recorded compatibility limits.
2. **Headless ingest core.** Scaffold the Cargo application/library, configuration and job schema, discovery/probing, collision-safe planning, copy/checksum, conversion, and output verification. Exit: a reproducible batch produces verified originals and editing copies with durable records.
3. **Desktop workflow.** Implement the approved dark-theme component system, folder pickers, remembered settings, source selection, preflight, progress table, error details, and cancellation. Cover every defined UI state. Exit: normal ingest is usable without a terminal, the UI remains responsive under load, and visual/accessibility checks pass.
4. **Recovery and retries.** Add startup reconciliation, verified-stage reuse within a batch, bounded transient retries, per-file retry, and report export. Exit: interrupted jobs recover without overwrites or unnecessary re-copying of verified archives.
5. **Release hardening.** Run real SD/NAS tests, document mounting and FFmpeg setup, choose Linux packaging, and add CI for formatting, linting, and meaningful core/integration tests. Exit: an installable build passes the acceptance cases below.

## Acceptance cases

- Folder selections survive restart; absent or changed mounts are clearly flagged.
- A representative batch preserves original hashes and produces playable, synchronized PCM MOVs in the actual Resolve installation.
- Unusual filenames, duplicate camera filenames, sidecars, nested folders, and no-audio clips behave predictably.
- Card removal, NAS disconnect, disk-full, permission errors, corrupt input, and forced process termination never mark incomplete outputs successful.
- Cancellation during every stage and retry after restarting reuse only verified work and never overwrite unrelated files.
- Archive byte mismatches fail verification. Truncated or undecodable editing files fail structural/decode checks; subsequent byte modification fails comparison against the recorded output checksum. Do not claim that decode success detects every possible content change.
- Failed probes can still be archived explicitly; archive-only sidecars have correct completion states and failed sidecars remain visible in batch totals.
- An unmounted or replaced archive/editing share cannot redirect writes into its local mountpoint; unsupported publication semantics fail preflight.
- Concurrent starts, path overlap, changed source files, and missing FFmpeg dependencies are handled before unsafe work occurs.
- Unit tests exercise state transitions, conflict planning, and estimates; integration tests use small generated clips and injected I/O failures. Real SMB interruption and Resolve import remain required manual tests.

## Decisions to confirm during the first milestone

The plan assumes an OS-mounted NAS and a separate editing destination, with audio-only conversion as the default. Confirm the user's actual mount paths, typical batch sizes, camera recording modes, and Resolve version/edition/GPU during setup. If unchanged H.264/H.265 video is unsupported, evaluate an explicit DNxHR conversion profile with its storage implications before expanding scope. Automatic card erasure, joining split clips, proxies, color grading, and direct Resolve integration are deferred.

## Technical references

- [Iced documentation](https://docs.rs/iced/latest/iced/index.html): GUI tasks and subscriptions.
- [FFmpeg documentation](https://ffmpeg.org/ffmpeg.html): explicit stream mapping, stream copy, metadata mapping, and progress output.
- [FFmpeg formats](https://ffmpeg.org/ffmpeg-formats.html): MOV container behavior.
- [FFmpeg codecs](https://ffmpeg.org/ffmpeg-codecs.html): decoder error detection.
- [FFprobe documentation](https://ffmpeg.org/ffprobe.html): structured media inspection.
- [Linux open documentation](https://www.man7.org/linux/man-pages/man2/open.2.html): directory handles and relative filesystem operations.
- [Linux fsync documentation](https://www.man7.org/linux/man-pages/man2/fsync.2.html): file and directory synchronization boundaries.
- [Blackmagic Resolve 20 codec matrix, July 2025](https://documents.blackmagicdesign.com/SupportNotes/DaVinci_Resolve_20_Supported_Codec_List.pdf): platform/edition codec restrictions. This is a reference baseline, not a guarantee for an unspecified installed version; validate the target installation during milestone 1.
