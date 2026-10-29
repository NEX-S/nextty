pub mod core;
pub mod terminal;
pub mod vt;

mod base64;
pub mod config;
pub mod error;
mod eventfd;
pub mod width;

#[cfg(feature = "files")]
pub mod files;
#[cfg(feature = "tui")]
pub mod history;
#[cfg(feature = "monitor")]
mod monitor;
#[cfg(any(feature = "gui", feature = "tui"))]
pub mod screen;
#[cfg(feature = "tui")]
mod shell_integration;
#[cfg(feature = "monitor")]
pub mod system_stats;
#[cfg(feature = "tui")]
pub mod tui;
#[cfg(feature = "tui")]
mod wire;
#[cfg(feature = "tui")]
pub mod workspace;

#[cfg(feature = "tui")]
mod bridge;
#[cfg(feature = "tui")]
mod clipboard_reader;

#[cfg(any(feature = "gui", feature = "tui"))]
mod input;
#[cfg(feature = "gui")]
mod native;
#[cfg(feature = "gui")]
mod paint;
#[cfg(all(feature = "gui", feature = "tui"))]
mod workspace_view;
#[cfg(all(feature = "gui", feature = "tui"))]
pub use workspace_view::WorkspaceView;

#[cfg(all(feature = "gui", not(feature = "tui")))]
mod desktop;
#[cfg(all(feature = "gui", not(feature = "tui")))]
pub use desktop::Desktop;

#[cfg(feature = "tui")]
mod search;
#[cfg(feature = "tui")]
mod ui;

#[cfg(all(feature = "tui", any(feature = "gui", feature = "agent")))]
mod context;

#[cfg(feature = "agent")]
mod agent_http;

#[cfg(feature = "agent")]
mod agent_context;

#[cfg(feature = "agent")]
mod harness;

#[cfg(feature = "agent")]
mod agent;

#[cfg(feature = "agent")]
mod markdown;

#[cfg(feature = "agent")]
pub mod agent_owner;
