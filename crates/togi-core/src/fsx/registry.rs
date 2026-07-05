//! Extension → language registry.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Language buckets files are batched into, one per adapter family.
///
/// Adding a language is source-level: add a variant here,
/// register its extensions in [`ExtensionRegistry::with_defaults`], and add
/// the adapter in the adapter registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// `.R` / `.r` — formatted and linted by air.
    R,
    /// `.py` and `.ipynb` — ruff handles notebooks natively.
    Python,
    /// `.qmd` / `.Rmd` — panache. Separate from [`Language::Markdown`] so the
    /// `[lint]`/`[format]` language lists can differ on plain md.
    Quarto,
    /// `.md` — panache; format-only by default.
    Markdown,
    /// `.sql` — sqlfluff.
    Sql,
}

impl Language {
    /// The language a `[format].languages` / `[lint].languages` config
    /// entry names (case-insensitive), or `None` for names togi does not
    /// know — the caller warns and skips those.
    pub fn from_config_name(name: &str) -> Option<Language> {
        match name.to_ascii_lowercase().as_str() {
            "r" => Some(Language::R),
            "python" => Some(Language::Python),
            "quarto" => Some(Language::Quarto),
            "markdown" => Some(Language::Markdown),
            "sql" => Some(Language::Sql),
            _ => None,
        }
    }
}

/// Maps file extensions to [`Language`] buckets.
///
/// Lookups are ASCII case-insensitive so `.R`/`.r` and `.Rmd`/`.rmd` land in
/// the same bucket. Extensions are stored without a leading dot.
#[derive(Debug, Clone)]
pub struct ExtensionRegistry {
    map: HashMap<String, Language>,
}

impl ExtensionRegistry {
    /// Registry with the default extension table.
    pub fn with_defaults() -> Self {
        let mut registry = Self {
            map: HashMap::new(),
        };
        registry.register("r", Language::R);
        registry.register("py", Language::Python);
        registry.register("ipynb", Language::Python);
        registry.register("qmd", Language::Quarto);
        registry.register("rmd", Language::Quarto);
        registry.register("md", Language::Markdown);
        registry.register("sql", Language::Sql);
        registry
    }

    /// Map `extension` (with or without a leading dot, any case) to `language`,
    /// replacing any existing mapping.
    pub fn register(&mut self, extension: &str, language: Language) {
        let key = extension.trim_start_matches('.').to_ascii_lowercase();
        self.map.insert(key, language);
    }

    /// The language bucket for `path`, or `None` if its extension is
    /// unregistered (such files are simply not format/lint targets).
    pub fn language_for(&self, path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?;
        self.map.get(&ext.to_ascii_lowercase()).copied()
    }
}

/// Batch `files` into per-language groups for the adapter runner.
///
/// Files with unregistered extensions are dropped. Input order is preserved
/// within each group; the map itself iterates in [`Language`] order.
pub fn group_by_language(
    files: &[PathBuf],
    registry: &ExtensionRegistry,
) -> BTreeMap<Language, Vec<PathBuf>> {
    let mut groups: BTreeMap<Language, Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        if let Some(language) = registry.language_for(file) {
            groups.entry(language).or_default().push(file.clone());
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn config_names_map_to_language_buckets() {
        let cases = [
            ("r", Language::R),
            ("python", Language::Python),
            ("quarto", Language::Quarto),
            ("markdown", Language::Markdown),
            ("sql", Language::Sql),
        ];
        for (name, lang) in cases {
            assert_eq!(
                Language::from_config_name(name),
                Some(lang),
                "config name {name}"
            );
        }
    }

    #[test]
    fn config_name_lookup_is_case_insensitive() {
        assert_eq!(Language::from_config_name("R"), Some(Language::R));
        assert_eq!(Language::from_config_name("Python"), Some(Language::Python));
    }

    #[test]
    fn unknown_config_names_are_none() {
        assert_eq!(Language::from_config_name("julia"), None);
        assert_eq!(Language::from_config_name(""), None);
    }

    #[test]
    fn registry_maps_spec_extensions_to_languages() {
        let reg = ExtensionRegistry::with_defaults();
        let cases = [
            ("analysis.R", Language::R),
            ("helpers.r", Language::R),
            ("model.py", Language::Python),
            ("notebook.ipynb", Language::Python),
            ("report.qmd", Language::Quarto),
            ("report.Rmd", Language::Quarto),
            ("notes.md", Language::Markdown),
            ("query.sql", Language::Sql),
        ];
        for (name, lang) in cases {
            assert_eq!(
                reg.language_for(Path::new(name)),
                Some(lang),
                "extension mapping for {name}"
            );
        }
    }

    #[test]
    fn registry_lookup_is_case_insensitive() {
        let reg = ExtensionRegistry::with_defaults();
        assert_eq!(reg.language_for(Path::new("a.PY")), Some(Language::Python));
        assert_eq!(reg.language_for(Path::new("a.rmd")), Some(Language::Quarto));
    }

    #[test]
    fn registry_returns_none_for_unknown_files() {
        let reg = ExtensionRegistry::with_defaults();
        assert_eq!(reg.language_for(Path::new("data.csv")), None);
        assert_eq!(reg.language_for(Path::new("Makefile")), None);
    }

    #[test]
    fn registry_is_extensible() {
        let mut reg = ExtensionRegistry::with_defaults();
        // New extension into an existing bucket.
        reg.register("pyi", Language::Python);
        assert_eq!(
            reg.language_for(Path::new("stubs.pyi")),
            Some(Language::Python)
        );
        // Re-registering an extension overrides the default mapping.
        reg.register("md", Language::Quarto);
        assert_eq!(
            reg.language_for(Path::new("notes.md")),
            Some(Language::Quarto)
        );
    }

    #[test]
    fn group_by_language_batches_files_per_adapter() {
        let reg = ExtensionRegistry::with_defaults();
        let files: Vec<PathBuf> = [
            "a.R", "b.r", "c.py", "d.qmd", "e.md", "f.sql", "g.ipynb", "data.csv",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        let groups = group_by_language(&files, &reg);

        assert_eq!(
            groups[&Language::R],
            vec![PathBuf::from("a.R"), PathBuf::from("b.r")]
        );
        assert_eq!(
            groups[&Language::Python],
            vec![PathBuf::from("c.py"), PathBuf::from("g.ipynb")]
        );
        assert_eq!(groups[&Language::Quarto], vec![PathBuf::from("d.qmd")]);
        assert_eq!(groups[&Language::Markdown], vec![PathBuf::from("e.md")]);
        assert_eq!(groups[&Language::Sql], vec![PathBuf::from("f.sql")]);
        // Unrecognized files are left out entirely.
        assert_eq!(groups.len(), 5);
    }
}
