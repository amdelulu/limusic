//! iOS PoToken fallback.
//!
//! `rustypipe-botguard` embeds deno_core/V8, which does not support the iOS target used by Tauri.
//! Returning no token lets the resolver fall back to clients that do not require BotGuard and
//! keeps V8 out of the iOS dependency graph.

use std::sync::Arc;
use std::time::Duration;

use crate::db::Db;

pub struct PoTokenGenerator;

impl PoTokenGenerator {
    pub fn new(_: Arc<Db>) -> Self {
        Self
    }

    pub async fn invalidate_session_token(&self) {}

    pub async fn get_session_po_token(&self, _: &str) -> Option<String> {
        None
    }

    pub async fn get_streaming_po_token(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    pub async fn prewarm(&self, _: &str) {}

    pub async fn teardown_if_idle(&self, _: Duration) {}
}
