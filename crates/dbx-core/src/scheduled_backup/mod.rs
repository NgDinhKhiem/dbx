//! Durable, UI-independent database backup scheduling shared by desktop and Web.
mod engine;
mod models;
mod service;
mod store;
#[cfg(test)]
mod tests;

pub use models::{BackupConfig, BackupFile, BackupRun, BackupSchedule, Migration, RunRequest};
pub use service::{BackupCommand, BackupService};
pub use store::{BackupSnapshot, BackupStore};

/// Metadata and export command chains nest very large async futures - a single
/// frame can reach 60-150 KiB - which does not fit into tokio's default 2 MiB
/// worker stack. Every runtime that drives them (desktop, background worker and
/// Web server) has to reserve the same roomy stack, otherwise the process dies
/// with `fatal runtime error: stack overflow` instead of reporting an error.
pub const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Upper bound for the blocking pool. `thread_stack_size` also applies to
/// blocking threads, so tokio's default of 512 could reserve 8 GiB of virtual
/// memory for stacks alone; 128 x 16 MiB keeps the worst case at 2 GiB while
/// leaving ample room for concurrent exports, imports and file IO.
pub const WORKER_MAX_BLOCKING_THREADS: usize = 128;

/// Name given to every runtime thread so crash reports and profilers can tell
/// them apart from driver and UI threads.
pub const WORKER_THREAD_NAME: &str = "dbx-worker";

/// Builds the multi-threaded runtime used by processes that run metadata and
/// backup work, on a stack that can hold those nested futures.
pub fn worker_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name(WORKER_THREAD_NAME)
        .thread_stack_size(WORKER_STACK_SIZE)
        .max_blocking_threads(WORKER_MAX_BLOCKING_THREADS)
        .build()
}
