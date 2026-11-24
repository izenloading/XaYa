//! Local service foundations. Engine adapters and persistence are added separately.

#[cfg(unix)]
mod acceptance;
#[cfg(unix)]
mod checkpoints;
#[cfg(unix)]
mod checks;
#[cfg(unix)]
pub mod client;
#[cfg(unix)]
pub mod cycle;

mod drive;
#[cfg(unix)]
mod events;
#[cfg(unix)]
mod faults;
#[cfg(unix)]
mod landings;
#[cfg(unix)]
mod ownership;
#[cfg(unix)]
mod projects;
#[cfg(unix)]
pub mod pty_host;
#[cfg(unix)]
mod reviews;
#[cfg(unix)]
pub mod runtime;
#[cfg(unix)]
pub mod server;
#[cfg(unix)]
pub mod sessions;
#[cfg(unix)]
mod storage;
#[cfg(unix)]
mod task_landing;
#[cfg(unix)]
mod tasks;
#[cfg(unix)]
pub use tasks::prompt as task_prompt;
#[cfg(unix)]
pub mod wire;
#[cfg(unix)]
mod workspaces;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid protocol data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("agent service error {code}: {message}")]
    Rpc { code: i32, message: String },
    #[error("{0}")]
    Protocol(&'static str),
    #[cfg(unix)]
    #[error("agent storage: {0}")]
    Storage(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
