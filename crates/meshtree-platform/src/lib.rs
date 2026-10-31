//! MeshTree platform layer: OS/process-boundary concerns (git plumbing,
//! IPC, locks, sharing links, teardown/quarantine, WAL) behind traits.
//! See `SPEC.md` Appendix B as amended by `docs/adr/0001-gitops-placement.md`.

pub mod coord;
pub mod gitops;
pub mod ipc;
pub mod lock;
pub mod share;
pub mod wal;
