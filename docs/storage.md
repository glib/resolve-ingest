# Supported storage and recovery

The archive and editing folders can be on a local disk, an external drive, or a network share that the operating system has mounted (kernel CIFS/SMB3 or NFS, for example through system settings or `/etc/fstab`). Keep any credentials file outside this application. Desktop `smb://` locations and other GVfs/FUSE paths are refused, because they cannot satisfy the publication and identity checks below. `findmnt -T /path/to/folder` shows what a folder is mounted on.

Each destination is remembered with the identity of the filesystem that holds it: the udisks mount point for removable drives (stable across `/dev/sdX` renumbering), otherwise the filesystem UUID where visible, otherwise the filesystem type, mount source, and mount root. Mount options are never stored. If a drive is unplugged or a share unmounted, the empty folder left behind does not match the remembered identity, and work is blocked instead of being written to local storage. A legitimate remount is accepted on retry after the roots are reopened and validated. The job journal itself must be on local storage.

## Permission denied during review

Review checks each destination by creating, syncing, renaming, and deleting small temporary files before copying any media. A folder can be browsable while these writes are denied. The error identifies the archive or editing destination that failed.

For CIFS, check both the local mount permissions and the NAS account's permissions on the selected folder. A mount presenting UID/GID 0 and directory mode `0755` allows only its owner to create files. Configure ownership for the desktop user's UID/GID in the host's CIFS mount configuration, retaining the existing authentication settings, and remount through the operating system. The NAS account must also be allowed to create, rename, and delete files. For a local editing destination, select a folder writable by the desktop user. After correcting access, choose **Check Again**.

The Linux implementation traverses directory descriptors with `openat`, `O_DIRECTORY`, and `O_NOFOLLOW`. Descents and source files must remain on the selected mount (including mount-ID checks for bind aliases). Conversion receives inherited, already-open file descriptors through `/proc/self/fd`; FFmpeg never opens a user-supplied output pathname. Files are created exclusively. Publication uses `renameat2(RENAME_NOREPLACE)` and checks file sync and directory sync. Preflight exercises successful publication and rejection of an existing target. Unsupported directory sync is recorded explicitly.

The archive checksum compares bytes streamed from the source with bytes read back through the client filesystem. This does not prove server-disk redundancy, server-side durability, or that a NAS has flushed its hardware cache. CIFS interruption/timeout behavior still needs validation on the actual server, kernel, and mount options. The UI remains on a separate thread while filesystem work runs; cancellation cannot guarantee interrupting blocked kernel I/O immediately.

## Concurrent processing

The separate **Convert existing files** workflow skips archival. It hashes the existing source, records input/output evidence, verifies the converted MOV, and publishes without overwriting. Outputs can sit beside the source (`clip.mp4.mov`) or under the configured editing destination, preserving relative folders. The original remains in place. An existing output blocks a new conversion plan; use History to recheck or retry the recorded conversion. This workflow requires no archive location or NAS connection. Its temporary output lives on the output filesystem, and it uses no extra staged original.

For local editing destinations, the pipeline copies the card independently to the archive and to a temporary original beside the editing output. These two reads can overlap; a slow card may limit the throughput benefit. The local copy receives checksum readback and identity checks before conversion. Its hash must match the verified archive before editing publication. Only one worker per lane runs, and no more than two assets are active. Preflight reserves the total output estimates plus the two largest selected editing originals for temporary local storage; staging and conversion also recheck available space at execution time.

Each attempt journals its archive, editing, and local-source temporary paths before creation. Successful local-source temporaries are removed after both branches finish. Failures retain owned partials for explicit cleanup; retries use new paths. Recovery can stage from a reverified archive when the card is absent. A shared editing destination uses the verified archive as conversion input and does not create a separate local-source cache. Cancellation stops new scheduling and waits for all active filesystem operations and supervised children before allowing retry or cleanup.

The two-asset limit bounds active work. Partials retained from failed attempts can consume additional space; free-space checks run again before staging and conversion, and History can remove those recorded partials once workers stop.

## Output paths

Originals go to `ARCHIVE/<recording day>/<camera filename>` and editing copies to `EDITING/<recording day>/<stem>.mov`; sidecars share their clip's day folder. The recording day comes from the filename timestamp, then embedded creation time, then file modification time. Names are kept readable. Characters that SMB or exFAT reject (`<>:"/\|?*` and control characters) become `_`, and trailing dots and spaces are dropped. Names are compared case-insensitively against files already in the folder and against the rest of the batch. A clash gets `-2`, `-3`, and so on, and a clip and its sidecars share the same suffix. Non-UTF8 or overlong names fail preflight with an explanation. Within a batch, the newest recordings are processed first.

Conversion-only batches keep the source's relative folder and append `.mov` to the full filename (`clip.mp4.mov`), so `clip.mp4` and `clip.mov` stay distinct beside their sources.

Batches from earlier versions keep the layout they were planned with (`DATE/BATCH/originals/…` with escaped names), so their retries still work.

## Recovery

The local SQLite journal snapshots settings, paths, expected hashes, verification checks, stage transitions, and attempts. Expected evidence is committed before publication. A crash after rename and before the state update can therefore be reconciled by checking the published file against that evidence. Existing files without matching evidence are conflicts, never implicit duplicates.

Retry rehashes any reusable archive. Existing editing results also undergo structural validation (plus a full decode when the batch was planned with the thorough check) and comparison with the recorded SHA-256. Retry Failed in the app re-runs only incomplete items; verified ones are not rechecked. A conversion can restart from a verified archive without the card. A source missing before archival must be reconnected at its recorded path; if the card's identity or mount path changed, rescan and start a new batch in this version.

Interrupted attempts restart with new, exclusively owned partial names. The journal retains earlier diagnostics. Only paths explicitly recorded as attempt partials can be cleaned up, and cleanup holds the application lock. FFmpeg inherits that lock and receives a parent-death signal: recovery cannot race an orphan that is still exiting. Cancellation sends SIGTERM, then escalates after two seconds if necessary, and waits for exit.

## Already-archived clips

Every verified original is indexed in the journal by archive folder and a source fingerprint: file size plus SHA-256 of the first and last MiB. Computing it reads at most 2 MiB per file, so a whole card can be compared with earlier ingests quickly. When a card is scanned, clips whose fingerprint matches a verified original that is still present at its recorded size are marked **Already archived** and left unselected. Selecting one anyway archives a second copy under a `-2` name. If the archive is unreachable during a scan, the card is still shown with everything selected, and preflight reports the archive problem. The fingerprint decides only what to preselect; every copy still gets a full SHA-256 and readback.

Report files are explicitly exported and never overwrite an existing report.
