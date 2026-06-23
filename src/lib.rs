//! Public Telegram bot that uploads images to Wikimedia Commons.
//!
//! Each user authenticates with their own Wikimedia Bot Password (scoped to the
//! Upload grant), and every image they send is uploaded to Commons under their
//! own account with a generated `{{Information}}` + license description.

pub mod app;
#[cfg(feature = "archive")]
pub mod archive;
pub mod aws;
pub mod commons;
pub mod config;
pub mod convert;
pub mod crypto;
pub mod geo;
pub mod metadata;
pub mod models;
pub mod oauth;
pub mod store;
pub mod telegram;
