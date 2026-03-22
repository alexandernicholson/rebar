use std::sync::Arc;

use dashmap::DashMap;

use super::types::AppError;

/// Per-application environment (configuration store).
///
/// Thread-safe key-value store backed by [`DashMap`].  Values are
/// stored as [`rmpv::Value`] so they can hold any MessagePack-compatible
/// type.
///
/// Modelled after Elixir/OTP's application environment.
#[derive(Debug, Clone)]
pub struct AppEnv {
    values: Arc<DashMap<String, rmpv::Value>>,
}

impl Default for AppEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl AppEnv {
    /// Create an empty application environment.
    #[must_use]
    pub fn new() -> Self {
        Self {
            values: Arc::new(DashMap::new()),
        }
    }

    /// Get a configuration value by key.
    ///
    /// Returns `None` if the key is not present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<rmpv::Value> {
        self.values.get(key).map(|v| v.value().clone())
    }

    /// Get a configuration value, returning `default` when the key is missing.
    #[must_use]
    pub fn get_or(&self, key: &str, default: rmpv::Value) -> rmpv::Value {
        self.values
            .get(key)
            .map_or(default, |v| v.value().clone())
    }

    /// Get a configuration value, returning an error when the key is missing.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::EnvKeyNotFound`] if `key` is not present.
    pub fn fetch(&self, key: &str) -> Result<rmpv::Value, AppError> {
        self.values
            .get(key)
            .map(|v| v.value().clone())
            .ok_or_else(|| AppError::EnvKeyNotFound(key.to_string()))
    }

    /// Set a configuration value at runtime.
    pub fn put(&self, key: &str, value: rmpv::Value) {
        self.values.insert(key.to_string(), value);
    }

    /// Delete a configuration value.
    pub fn delete(&self, key: &str) {
        self.values.remove(key);
    }

    /// Return all key-value pairs in the environment.
    ///
    /// The ordering is not guaranteed.
    #[must_use]
    pub fn all(&self) -> Vec<(String, rmpv::Value)> {
        self.values
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_get_set() {
        let env = AppEnv::new();
        env.put("key", rmpv::Value::String("value".into()));
        let val = env.get("key").unwrap();
        assert_eq!(val.as_str(), Some("value"));
    }

    #[test]
    fn env_get_missing() {
        let env = AppEnv::new();
        assert!(env.get("missing").is_none());
    }

    #[test]
    fn env_get_or_default() {
        let env = AppEnv::new();
        let val = env.get_or("missing", rmpv::Value::Integer(42.into()));
        assert_eq!(val.as_u64(), Some(42));
    }

    #[test]
    fn env_get_or_present() {
        let env = AppEnv::new();
        env.put("key", rmpv::Value::Integer(1.into()));
        let val = env.get_or("key", rmpv::Value::Integer(42.into()));
        assert_eq!(val.as_u64(), Some(1));
    }

    #[test]
    fn env_fetch_missing_errors() {
        let env = AppEnv::new();
        let err = env.fetch("missing").unwrap_err();
        assert!(matches!(err, AppError::EnvKeyNotFound(_)));
    }

    #[test]
    fn env_fetch_present() {
        let env = AppEnv::new();
        env.put("key", rmpv::Value::Boolean(true));
        let val = env.fetch("key").unwrap();
        assert_eq!(val.as_bool(), Some(true));
    }

    #[test]
    fn env_delete() {
        let env = AppEnv::new();
        env.put("key", rmpv::Value::Nil);
        assert!(env.get("key").is_some());
        env.delete("key");
        assert!(env.get("key").is_none());
    }

    #[test]
    fn env_all() {
        let env = AppEnv::new();
        env.put("a", rmpv::Value::Integer(1.into()));
        env.put("b", rmpv::Value::Integer(2.into()));
        let mut pairs = env.all();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "a");
        assert_eq!(pairs[1].0, "b");
    }

    #[test]
    fn env_overwrite() {
        let env = AppEnv::new();
        env.put("key", rmpv::Value::Integer(1.into()));
        env.put("key", rmpv::Value::Integer(2.into()));
        assert_eq!(env.get("key").unwrap().as_u64(), Some(2));
    }

    #[test]
    fn env_concurrent_access() {
        use std::sync::Arc;

        let env = Arc::new(AppEnv::new());
        let mut handles = Vec::new();

        for i in 0..10 {
            let env = Arc::clone(&env);
            handles.push(std::thread::spawn(move || {
                let key = format!("key_{i}");
                env.put(&key, rmpv::Value::Integer(i.into()));
                // Read back our own key.
                assert!(env.get(&key).is_some());
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(env.all().len(), 10);
    }

    #[test]
    fn env_default_is_empty() {
        let env = AppEnv::default();
        assert!(env.all().is_empty());
    }
}
