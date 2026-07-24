//! Process-local coordination for logical Provider references stored outside
//! SQLite, such as companion and public-agent JSON side stores.
//!
//! A read guard covers a side-store write plus its Provider existence check.
//! A write guard covers the complete Provider deletion scan and database
//! delete. This is deliberately an application lock, not a physical foreign
//! key, trigger, or database cascade.

use std::sync::Arc;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Process-local barrier for Provider catalog lifecycle operations.
#[derive(Debug, Default)]
pub struct ProviderLifecycleBarrier {
    lock: RwLock<()>,
}

impl ProviderLifecycleBarrier {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.read().await
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, ()> {
        self.lock.write().await
    }
}

pub type SharedProviderLifecycleBarrier = Arc<ProviderLifecycleBarrier>;
