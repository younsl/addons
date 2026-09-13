//! Syncs the SQLite metadata database to S3 for the object-storage HA mode.
//! SQLite cannot run live on S3 (it needs POSIX file locking), so the live
//! database stays on a local volume (typically an emptyDir) and this module
//! keeps a durable copy in S3: the leader periodically uploads a `VACUUM INTO`
//! snapshot, and every pod restores the latest snapshot on boot and applies it
//! on promotion.
//!
//! It reuses [`meta::Store::snapshot`] / [`meta::Store::swap_from_snapshot`]
//! and mirrors the leader/standby control flow of the PV-based replicator. The
//! tradeoff is the same: replication is asynchronous, so a crash can lose the
//! writes made since the last cycle (an orderly demotion or shutdown flushes a
//! final snapshot).
//!
//! The invariant that keeps this safe is that S3 holds the only authoritative
//! copy: a leader always promotes onto the object that is current at that
//! moment, never onto its own local database. The database may therefore fall
//! behind, but it never moves backwards from what S3 already published -- which
//! is what stops a resurrected artifact row from outliving blob bytes the
//! sweeper reclaimed.

mod metasync;

pub use metasync::{
    Error, GetObjectOutput, HeadObjectOutput, MetaOptions, MetaSync, ObjectApi, PutBody,
    PutObjectInput, PutObjectOutput, Result, S3Api,
};
