# Supported storage and recovery

The operating system mounts and authenticates the NAS. Use an actual kernel CIFS mount, for example a mount maintained through your distribution's system settings or `/etc/fstab`. Keep any credentials file outside this application, readable only by its owner. Use `findmnt -T /mnt/nas/archive` to check that the location is on `cifs` or `smb3` before selecting it. Desktop `smb://` locations, GVfs/FUSE, and NFS are outside this storage profile.

Choose existing, independent source, original-archive, and editing roots. The archive must be CIFS in the desktop; the editing root can be local or CIFS. The application remembers filesystem type, share source, and mount root separately from the path. It does not retain mount options. If a share is unmounted, its local fallback directory will not match the remembered identity and ingest is blocked. A legitimate remount is accepted on manual retry after reopening and validating the roots.

## Permission denied during review

Review checks each destination by creating, syncing, renaming, and deleting small temporary files before copying any media. A folder can be browsable while these writes are denied. The error identifies the archive or editing destination that failed.

For CIFS, check both the local mount permissions and the NAS account's permissions on the selected folder. A mount presenting UID/GID 0 and directory mode `0755` allows only its owner to create files. Configure ownership for the desktop user's UID/GID in the host's CIFS mount configuration, retaining the existing authentication settings, and remount through the operating system. The NAS account must also be allowed to create, rename, and delete files. For a local editing destination, select a folder writable by the desktop user. Retry Review ingest after correcting access.

The Linux implementation traverses directory descriptors with `openat`, `O_DIRECTORY`, and `O_NOFOLLOW`. Descents and source files must remain on the selected mount (including mount-ID checks for bind aliases). Conversion receives inherited, already-open file descriptors through `/proc/self/fd`; FFmpeg never opens a user-supplied output pathname. Files are created exclusively. Publication uses `renameat2(RENAME_NOREPLACE)` and checks file sync and directory sync. Preflight exercises successful publication and rejection of an existing target. Unsupported directory sync is recorded explicitly.

The archive checksum compares bytes streamed from the source with bytes read back through the client filesystem. This does not prove server-disk redundancy, server-side durability, or that a NAS has flushed its hardware cache. CIFS interruption/timeout behavior still needs validation on the actual server, kernel, and mount options. The UI remains on a separate thread while filesystem work runs; cancellation cannot guarantee interrupting blocked kernel I/O immediately.

## Concurrent processing

The separate **Convert existing files** workflow skips archival. It hashes the existing source, records input/output evidence, verifies the converted MOV, and publishes without overwriting. Outputs can sit beside the source (`clip.mp4.mov`) or under the configured editing destination, preserving relative folders. The original remains in place. An existing output blocks a new conversion plan; use History to recheck or retry the recorded conversion. This workflow requires no archive location or NAS connection. Its temporary output lives on the output filesystem, and it uses no extra staged original.

For local editing destinations, the pipeline copies the card independently to the archive and to a temporary original beside the editing output. These two reads can overlap; a slow card may limit the throughput benefit. The local copy receives checksum readback and identity checks before conversion. Its hash must match the verified archive before editing publication. Only one worker per lane runs, and no more than two assets are active. Preflight reserves the total output estimates plus the two largest selected editing originals for temporary local storage; staging and conversion also recheck available space at execution time.

Each attempt journals its archive, editing, and local-source temporary paths before creation. Successful local-source temporaries are removed after both branches finish. Failures retain owned partials for explicit cleanup; retries use new paths. Recovery can stage from a reverified archive when the card is absent. A shared editing destination uses the verified archive as conversion input and does not create a separate local-source cache. Cancellation stops new scheduling and waits for all active filesystem operations and supervised children before allowing retry or cleanup.

The two-asset limit bounds active work. Partials retained from failed attempts can consume additional space; free-space checks run again before staging and conversion, and History can remove those recorded partials once workers stop.

## Output paths

Each batch gets a UTC date and UUID. Source hierarchy is preserved under `DATE/UUID/originals` and `DATE/UUID/clips`. Components use reversible ASCII escaping: `f-` prefix, lowercase ASCII letters/digits/hyphen/underscore unchanged, every other UTF-8 byte encoded as `~hh`. Uppercase is escaped so case-folding destinations cannot collapse distinct source names. The editing name retains the encoded original extension and appends `.mov`, so `clip.mp4` and `clip.mov` stay separate. The manifest maps original names to planned names. Overlong and non-UTF8 paths fail preflight with an explanation. Files are never silently renamed to an ambiguous destination name.

## Recovery

The local SQLite journal snapshots settings, paths, expected hashes, verification checks, stage transitions, and attempts. Expected evidence is committed before publication. A crash after rename and before the state update can therefore be reconciled by checking the published file against that evidence. Existing files without matching evidence are conflicts, never implicit duplicates.

Retry rehashes any reusable archive. Existing editing results also undergo structural validation, full-stream decode, and comparison with the recorded SHA-256. A conversion can restart from a verified archive without the card. A source missing before archival must be reconnected at its recorded path; if the card's identity or mount path changed, rescan and start a new batch in this version.

Interrupted attempts restart with new, exclusively owned partial names. The journal retains earlier diagnostics. Only paths explicitly recorded as attempt partials can be cleaned up, and cleanup holds the application lock. FFmpeg inherits that lock and receives a parent-death signal: recovery cannot race an orphan that is still exiting. Cancellation sends SIGTERM, then escalates after two seconds if necessary, and waits for exit.

New batches always create new archive/output sets. There is no cross-batch deduplication. Report files are explicitly exported and never overwrite an existing report.
