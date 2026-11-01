pub mod core;
pub mod poll;
pub mod terminal;
pub mod vt;

mod base64;
pub mod config;
pub mod error;
mod eventfd;
pub mod width;

pub mod files;

pub mod history;

mod monitor;

pub mod screen;

mod shell_integration;

pub mod system_stats;

pub mod tui;

mod wire;

pub mod workspace;

mod bridge;

mod clipboard_reader;

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

mod file_panel;
mod panel;
mod ui;

#[cfg(any(feature = "gui", feature = "agent"))]
mod context;

#[cfg(feature = "agent")]
pub mod agent;
