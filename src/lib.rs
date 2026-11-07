pub mod core;
pub mod poll;
pub mod terminal;
pub mod vt;

mod base64;
pub mod config;
pub mod error;
mod eventfd;
pub mod width;

pub(crate) mod agent_files;
pub mod files;
mod worker;

pub mod history;

#[cfg(any(feature = "gui", feature = "tui"))]
mod monitor;

pub mod screen;

mod shell_integration;

pub mod system_stats;

#[cfg(feature = "tui")]
pub mod tui;

mod wire;

pub mod workspace;

mod bridge;

mod clipboard_reader;

#[cfg(any(feature = "gui", feature = "tui"))]
mod input;
#[cfg(feature = "gui")]
mod native;
#[cfg(feature = "gui")]
mod paint;
#[cfg(feature = "gui")]
mod workspace_view;
#[cfg(feature = "gui")]
pub use workspace_view::WorkspaceView;

mod search;

#[cfg(any(feature = "gui", feature = "tui"))]
mod file_panel;
#[cfg(any(feature = "gui", feature = "tui"))]
mod panel;
#[cfg(any(feature = "gui", feature = "tui"))]
mod ui;

#[cfg(any(feature = "gui", feature = "tui"))]
mod context;

pub mod agent;
