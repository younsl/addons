# WAL shipping

| Status | Category |
| --- | --- |
| implemented in 0.18.0 | storage |

The s3 backend publishes metadata changes as append-only WAL segments on top of a base copy of the SQLite file, instead of re-uploading the whole database on every change.

## Why

![before and after](wal-shipping-why.svg)

| Measured (shared cluster, 2026-10-21) | Value |
| --- | --- |
| Metadata snapshot | 4.7 GB, doubled in 30 days |
| Snapshot uploads | 167 a day, 12 an hour in business hours |
| Bytes overwritten on one key | about 770 GB a day |
| SeaweedFS data PVC | 19 to 38 GiB every 30 minutes, 80% alert flapping |

SeaweedFS is append-only. An overwritten object stays on disk until vacuum, so the upload rate, not the data, drove the volume.

## Architecture

![architecture](wal-shipping-architecture.svg)

## Generation lifecycle

![lifecycle](wal-shipping-lifecycle.svg)

## Invariants

| Invariant | Enforced by |
| --- | --- |
| Only one connection writes | Read pool opens `query_only` |
| No frame is checkpointed unread | `wal_autocheckpoint = 0`, the shipper checkpoints only after reading every commit, holding the write connection |
| Base matches its generation's first frame | Base is copied right after `TRUNCATE`, and the file changes only at the next checkpoint |
| Only committed, intact frames ship | Salt and cumulative checksum validated like SQLite recovery, cut at the last commit frame |
| Replay is exact | Base plus frames equals the checkpointed file byte for byte (tested with VACUUM and schema changes) |
| No gap is ever applied | Segments upload in order, readers stop at the first missing sequence, each segment carries its first frame index |
| A stale leader cannot win | Base PUT is fenced (term in metadata, `If-Match`), segment PUT is `If-None-Match: *`, every term starts its own generation |
| A bad copy never serves | SHA-256 on base and segments, `quick_check` before boot and promotion, promotion fails closed |

## Failure handling

| Event | Behavior |
| --- | --- |
| Bucket unreachable | Segments queue in order (256 MiB bound), then upload. Past the bound, the generation is replaced. |
| Lost PUT response | Retry gets 412, identical bytes count as shipped |
| Foreign segment at our key | Generation abandoned, next base carries the changes |
| WAL checkpointed or reset outside the shipper | Detected by salt and length, generation abandoned and replaced in the same cycle |
| Reader blocks `TRUNCATE` | Generation kept, retried next cycle. A WAL restart afterwards is safe because every frame was read. |
| Writers outpace one cycle | Capture continues for up to 64 rounds, the rest next cycle |
| Demotion, shutdown | Final flush captures until the WAL is drained |

## Bounds

| Setting | Value |
| --- | --- |
| Ship cadence (RPO) | 5 s |
| Segment | 16 MiB of pages, zstd |
| Rotation | segments outgrow the base (64 MiB floor), 2000 segments, a lost frame, or a new term |
| Leader checkpoint | WAL over 64 MiB and fully shipped |

## Compatibility

| Case | Result |
| --- | --- |
| Upgrade from a plain snapshot | Read as before, the new leader starts a generation |
| `metaSyncMode: snapshot` | Previous behavior. Readers follow either layout, so the mode can change between terms. |
| Roll back below 0.18.0 | Set `metaSyncMode: snapshot` and `metaCompression: none`, let one upload finish, then roll back |
| Storage migration | Rebuilds base plus segments into one file, uploads it as a plain snapshot |

## Validation

- `cargo test walfmt walship`: format, replay, rollback, corruption, reset, conflicts, mode switch, fencing, and concurrent writes with a promoted standby compared row for row.
- `forklift_objstore_meta_wal_pending_bytes` stays near 0, `forklift_objstore_meta_wal_segments_total{result="error"}` stays flat.
- SeaweedFS data PVC stops swinging with business hours.
