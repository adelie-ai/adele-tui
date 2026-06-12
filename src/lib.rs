//! Terminal UI client library for the Adelie AI platform.
//!
//! Exposes the screens and widgets that make up the `adele` TUI (chat, task
//! pane, knowledge base, connections, purposes, model selection, key bindings,
//! and supporting credential/OAuth helpers) so they can be reused and tested
//! independently of the binary entry point.

pub mod app;
pub mod client_tools;
pub mod connections;
pub mod credentials;
pub mod in_flight;
pub mod kb;
pub mod keys;
pub mod markdown;
pub mod model_selector;
pub mod oauth;
pub mod picker;
pub mod profile;
pub mod purposes;
pub mod settings;
pub mod tasks;
pub mod toolbar;
pub mod ui;
pub mod voice;
