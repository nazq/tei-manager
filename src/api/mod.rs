//! REST API module

pub mod handlers;
pub mod models;
pub mod routes;

pub use routes::{AppState, create_router};
