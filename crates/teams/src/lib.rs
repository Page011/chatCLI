//! Microsoft Teams through Microsoft Graph, delegated (signed-in user) permissions only.

pub mod auth;
pub mod backend;
pub mod body;
pub mod doctor;
pub mod graph;
pub mod html;
pub mod map;
pub mod relay;
pub mod subscription;
pub mod transfer;

#[cfg(test)]
mod testkit;

pub use backend::run;
pub use relay::{relay_set_url, relay_setup};
