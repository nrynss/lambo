//! Unit tests for the session proxy, grouped by subject.

use super::*;
use crate::store::StoreConfig;
use chrono::Utc;

mod dialing;
mod disconnect;
mod forwarding;
mod proxyable;

fn ours() -> SessionEndpoint {
    let store = StoreConfig {
        kind: crate::store::StoreKind::Sqlite,
        path: Some("/a.db".into()),
        ..StoreConfig::default()
    };
    SessionEndpoint::for_store("s", &store).expect("a file-backed store is shareable")
}
