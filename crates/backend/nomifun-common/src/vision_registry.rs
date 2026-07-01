//! 进程级"模型不支持图片"记忆表(仅内存,随进程退出清空)。
//!
//! key = provider_id + model(同名模型跨 provider 隔离)。发送侧(工厂构建
//! Config 时)读、会话服务错误兜底时写。用全局单例避免把 Arc 穿过整条工厂
//! 依赖链;单测用 `new()` 独立实例。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
pub struct VisionUnsupportedRegistry {
    inner: Mutex<HashSet<String>>,
}

impl VisionUnsupportedRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(provider_id: &str, model: &str) -> String {
        format!("{provider_id}\u{1f}{model}")
    }

    /// 记住该 (provider_id, model) 不支持图片输入。幂等。
    pub fn mark_unsupported(&self, provider_id: &str, model: &str) {
        if let Ok(mut set) = self.inner.lock() {
            set.insert(Self::key(provider_id, model));
        }
    }

    /// 该 (provider_id, model) 是否已被标记为不支持图片。锁中毒时按 false(fail-open)。
    pub fn is_unsupported(&self, provider_id: &str, model: &str) -> bool {
        self.inner
            .lock()
            .map(|set| set.contains(&Self::key(provider_id, model)))
            .unwrap_or(false)
    }

    /// 进程级共享单例。
    pub fn global() -> &'static VisionUnsupportedRegistry {
        static GLOBAL: OnceLock<VisionUnsupportedRegistry> = OnceLock::new();
        GLOBAL.get_or_init(VisionUnsupportedRegistry::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_then_is_unsupported_hits() {
        let reg = VisionUnsupportedRegistry::new();
        assert!(!reg.is_unsupported("p1", "gpt-x"));
        reg.mark_unsupported("p1", "gpt-x");
        assert!(reg.is_unsupported("p1", "gpt-x"));
    }

    #[test]
    fn same_model_different_provider_is_isolated() {
        let reg = VisionUnsupportedRegistry::new();
        reg.mark_unsupported("p1", "m");
        assert!(reg.is_unsupported("p1", "m"));
        assert!(!reg.is_unsupported("p2", "m"));
    }

    #[test]
    fn global_is_shared_singleton() {
        let a = VisionUnsupportedRegistry::global() as *const _;
        let b = VisionUnsupportedRegistry::global() as *const _;
        assert_eq!(a, b);
    }
}
