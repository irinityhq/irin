//! Flag `#[derive(..., Debug, ...)]` on signing / key-material types.
//!
//! Matches:
//! - type name ~ `(?i)(SigningKey|KeyMaterial|SecretKey|PrivateKey|LedgerKey)`
//! - OR a field whose type path ends in `SigningKey`
//!
//! Implemented as an **early** lint so `#[derive(Debug)]` is still present
//! (derive expands before late lints see the ADT).

use clippy_utils::diagnostics::span_lint_and_help;
use rustc_ast::{Attribute, Item, ItemKind, TyKind, VariantData};
use rustc_lint::{EarlyContext, EarlyLintPass};
use rustc_session::{declare_lint, declare_lint_pass};
use rustc_span::sym;

use crate::is_key_material_name;

declare_lint! {
    /// ### What it does
    ///
    /// Flags `#[derive(Debug)]` on signing-key / secret key material types.
    ///
    /// ### Why is this bad?
    ///
    /// Debug formatting can leak key bytes into logs, panics, and error reports.
    /// Prefer an explicit redacting `Debug` impl (or no Debug at all).
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// #[derive(Debug)]
    /// struct SigningKey([u8; 32]);
    /// ```
    ///
    /// Use instead: omit Debug, or implement a redacting Debug.
    pub NO_DEBUG_ON_SIGNING_KEY_TYPES,
    Warn,
    "do not derive Debug on signing/key-material types"
}

declare_lint_pass!(NoDebugOnSigningKeyTypes => [NO_DEBUG_ON_SIGNING_KEY_TYPES]);

pub fn register(lint_store: &mut rustc_lint::LintStore) {
    lint_store.register_lints(&[NO_DEBUG_ON_SIGNING_KEY_TYPES]);
    // Pre-expansion: `#[derive(Debug)]` is stripped/expanded before early/late passes.
    lint_store.register_pre_expansion_pass(|| Box::new(NoDebugOnSigningKeyTypes));
}

impl EarlyLintPass for NoDebugOnSigningKeyTypes {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &Item) {
        let (ident, data_list) = match &item.kind {
            ItemKind::Struct(ident, _, data) | ItemKind::Union(ident, _, data) => {
                (ident, vec![data])
            }
            ItemKind::Enum(ident, _, def) => {
                (ident, def.variants.iter().map(|v| &v.data).collect())
            }
            _ => return,
        };

        let name_hit = is_key_material_name(ident.as_str());
        let field_hit = data_list.iter().any(|d| variant_has_signing_key_field(d));
        if !(name_hit || field_hit) {
            return;
        }
        if !attrs_derive_trait(&item.attrs, "Debug") {
            return;
        }

        span_lint_and_help(
            cx,
            NO_DEBUG_ON_SIGNING_KEY_TYPES,
            ident.span,
            format!(
                "deriving `Debug` on key-material type `{}` can leak secrets via logs and panics",
                ident.as_str()
            ),
            None,
            "remove `Debug` from the derive list, or implement a redacting `Debug` manually",
        );
    }
}

pub(crate) fn attrs_derive_trait(attrs: &[Attribute], trait_name: &str) -> bool {
    for attr in attrs {
        if !attr.has_name(sym::derive) {
            continue;
        }
        let Some(list) = attr.meta_item_list() else {
            continue;
        };
        for meta in list {
            if let Some(ident) = meta.ident()
                && ident.name.as_str() == trait_name
            {
                return true;
            }
            // path form: `core::fmt::Debug`
            if let Some(path) = meta.meta_item().map(|m| &m.path)
                && path
                    .segments
                    .last()
                    .is_some_and(|s| s.ident.name.as_str() == trait_name)
            {
                return true;
            }
        }
    }
    false
}

fn variant_has_signing_key_field(data: &VariantData) -> bool {
    data.fields().iter().any(|field| ty_ends_with_signing_key(&field.ty.kind))
}

fn ty_ends_with_signing_key(kind: &TyKind) -> bool {
    match kind {
        TyKind::Path(None, path) => path
            .segments
            .last()
            .is_some_and(|s| s.ident.as_str().ends_with("SigningKey")),
        TyKind::Path(Some(qself), _) => ty_ends_with_signing_key(&qself.ty.kind),
        TyKind::Ref(_, mut_ty) => ty_ends_with_signing_key(&mut_ty.ty.kind),
        TyKind::Slice(inner) | TyKind::Array(inner, _) | TyKind::Paren(inner) => {
            ty_ends_with_signing_key(&inner.kind)
        }
        _ => false,
    }
}
