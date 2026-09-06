#[cfg(not(target_os = "linux"))]
compile_error!("Resolve Ingest currently supports Linux only");

pub mod catalog;
pub mod diagnostics;
pub mod discovery;
pub mod errors;
pub mod jobs;
pub mod media;
pub mod model;
pub mod pipeline;
pub mod settings;
pub mod storage;
#[cfg(feature = "desktop")]
pub mod ui;
pub mod verification;
