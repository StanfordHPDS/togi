//! Language bucket → adapter instance registry.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::adapters::{Adapter, RuffAdapter};
use crate::fsx::Language;

/// Maps [`Language`] buckets (from the fsx extension registry) to the
/// adapter that handles them — the one place to wire up a language.
///
/// Several buckets may share one adapter instance (e.g. Quarto and
/// Markdown both go to the markdown formatter); the batch runner merges
/// their files into a single invocation by adapter name.
#[derive(Default)]
pub struct AdapterRegistry {
    map: BTreeMap<Language, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    /// An empty registry; real adapters register themselves here as they
    /// are wired into the commands.
    pub fn new() -> AdapterRegistry {
        AdapterRegistry::default()
    }

    /// The production registry: every language bucket that has a real
    /// adapter, pre-wired. New adapters add their line here.
    pub fn with_defaults() -> AdapterRegistry {
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::new(RuffAdapter));
        registry.register(Language::R, Arc::new(crate::adapters::AirAdapter));
        let panache: Arc<dyn Adapter> = Arc::new(crate::adapters::PanacheAdapter::new());
        registry.register(Language::Quarto, Arc::clone(&panache));
        registry.register(Language::Markdown, panache);
        registry.register(Language::Sql, Arc::new(crate::adapters::SqlFluffAdapter));
        registry
    }

    /// Route `language` to `adapter`, replacing any existing routing.
    pub fn register(&mut self, language: Language, adapter: Arc<dyn Adapter>) {
        self.map.insert(language, adapter);
    }

    /// The adapter handling `language`, or `None` if the language has no
    /// adapter (its files are simply skipped).
    pub fn adapter_for(&self, language: Language) -> Option<&Arc<dyn Adapter>> {
        self.map.get(&language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::FakeAdapter;

    #[test]
    fn with_defaults_routes_python_to_the_ruff_adapter() {
        let registry = AdapterRegistry::with_defaults();
        let adapter = registry
            .adapter_for(Language::Python)
            .expect("python has a real adapter");
        assert_eq!(adapter.name(), "ruff");
    }

    #[test]
    fn with_defaults_routes_r_files_to_air() {
        let registry = AdapterRegistry::with_defaults();
        let adapter = registry
            .adapter_for(Language::R)
            .expect("R is a built-in bucket");
        assert_eq!(adapter.name(), "air");
    }

    #[test]
    fn with_defaults_routes_sql_files_to_sqlfluff() {
        let registry = AdapterRegistry::with_defaults();
        let adapter = registry
            .adapter_for(Language::Sql)
            .expect("sql ships with an adapter");
        assert_eq!(adapter.name(), "sqlfluff");
    }

    #[test]
    fn adapter_for_returns_the_registered_adapter() {
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Python, Arc::new(FakeAdapter::new("ruff")));

        let adapter = registry
            .adapter_for(Language::Python)
            .expect("python was registered");
        assert_eq!(adapter.name(), "ruff");
        assert!(registry.adapter_for(Language::Sql).is_none());
    }

    #[test]
    fn registering_a_language_twice_replaces_the_adapter() {
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Sql, Arc::new(FakeAdapter::new("old")));
        registry.register(Language::Sql, Arc::new(FakeAdapter::new("new")));

        let adapter = registry.adapter_for(Language::Sql).expect("registered");
        assert_eq!(adapter.name(), "new");
    }

    #[test]
    fn defaults_route_quarto_and_markdown_to_one_panache_instance() {
        let registry = AdapterRegistry::with_defaults();
        let quarto = registry
            .adapter_for(Language::Quarto)
            .expect("quarto is wired by default");
        let markdown = registry
            .adapter_for(Language::Markdown)
            .expect("markdown is wired by default");
        assert_eq!(quarto.name(), "panache");
        // The same instance serves both buckets, so the runner merges
        // their files into one panache invocation.
        assert!(Arc::ptr_eq(quarto, markdown));
    }

    #[test]
    fn one_adapter_instance_can_serve_several_buckets() {
        let markdown: Arc<dyn Adapter> = Arc::new(FakeAdapter::new("panache"));
        let mut registry = AdapterRegistry::new();
        registry.register(Language::Quarto, Arc::clone(&markdown));
        registry.register(Language::Markdown, Arc::clone(&markdown));

        for language in [Language::Quarto, Language::Markdown] {
            let adapter = registry.adapter_for(language).expect("registered");
            assert_eq!(adapter.name(), "panache");
        }
    }
}
