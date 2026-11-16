//! Microsoft Teams through Microsoft Graph, delegated (signed-in user) permissions only.

pub mod auth;
pub mod backend;
pub mod graph;
pub mod html;
pub mod map;
pub mod relay;
pub mod subscription;

pub use backend::run;
pub use relay::relay_setup;
