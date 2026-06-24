//! Semantic provider seam (LSP sidecar — Phase 1; see `docs/design-lsp-sidecar.md`).
//!
//! vyer's graph (`refs`/`impact`/`context`/blast-radius/safe-delete) is today a
//! lexical + tree-sitter APPROXIMATION. True type-resolved semantics need a
//! language server. This module is the seam that a real `LspProvider` (Phase 2+)
//! plugs into; the default [`NullProvider`] returns nothing, so the engine
//! transparently falls back to its approximation and reports the tier honestly
//! (Rule §8: degrade, don't crash; always disclose the tier so the agent calibrates).
//!
//! No external dependency is introduced here — only the trait + the honest-default
//! provider + tier reporting. The heavy rust-analyzer/tsserver integration is the
//! next phase, behind `--allow-lsp` and an operator allowlist (Rule §9).

/// How resolved the semantic answer is — surfaced to the agent so it can calibrate
/// trust (a `partial` ref list may miss type-resolved call sites; `none` means the
/// lexical/tree-sitter approximation is in use).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Type-resolved by a language server (cross-file, scope-aware).
    Full,
    /// Lexical + tree-sitter approximation (vyer's built-in graph).
    Partial,
    /// No semantic resolution available for this language/target.
    None,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Full => "lsp",
            Tier::Partial => "lexical-approx",
            Tier::None => "none",
        }
    }
}

/// A type-resolved reference (Phase 2+ fills these). Kept minimal and transport-
/// agnostic so the engine maps it onto the existing span/locator output.
#[derive(Debug, Clone)]
pub struct SemRef {
    pub path: String,
    pub line: u32,
}

/// The seam every semantic backend implements. The engine consults it as an
/// UPGRADE: it computes its approximation first (so a `None`/timeout costs nothing),
/// then a `Some(..)` result replaces it and bumps the reported tier. Every method
/// is best-effort and MUST NOT panic or block indefinitely (Rule §8).
pub trait SemanticProvider: Send + Sync {
    /// Type-resolved references to the symbol named `name` defined in `def_file`.
    /// `None` → no semantic answer; caller falls back to the lexical approximation.
    fn references(&self, _def_file: &str, _name: &str) -> Option<Vec<SemRef>> {
        None
    }
    /// The semantic tier currently available (drives the honest `tier=` label).
    fn tier(&self) -> Tier {
        Tier::None
    }
    /// A short status string for `vyer://status` (e.g. "off" / "rust-analyzer").
    fn status(&self) -> String {
        "off (lexical/tree-sitter approximation)".to_string()
    }
}

/// The default: no language server. The engine's built-in approximation is used and
/// honestly reported as `tier=lexical-approx`. Selecting a real provider is Phase 2.
#[derive(Debug, Default)]
pub struct NullProvider;

impl SemanticProvider for NullProvider {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_provider_degrades_honestly() {
        let p = NullProvider;
        assert!(p.references("src/a.rs", "foo").is_none());
        assert_eq!(p.tier(), Tier::None);
        assert_eq!(Tier::Partial.label(), "lexical-approx");
        assert_eq!(Tier::Full.label(), "lsp");
    }
}
