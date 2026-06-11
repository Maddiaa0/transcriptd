pub mod config;
pub mod extract;
pub mod openrouter;
pub mod scan;
pub mod state;
pub mod watch;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
