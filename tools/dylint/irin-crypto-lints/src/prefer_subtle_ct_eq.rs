//! Prefer constant-time equality for byte comparisons on auth/verify surfaces.
//!
//! Flags:
//! 1. `==` / `!=` between byte-ish values in functions named like
//!    `verify|compare|auth|arm|token|mac|sig` (late pass).
//! 2. `#[derive(PartialEq)]` on key-material type names (early pass).

use clippy_utils::diagnostics::span_lint_and_help;
use rustc_ast::Item as AstItem;
use rustc_ast::ItemKind as AstItemKind;
use rustc_hir::{BinOpKind, Expr, ExprKind, HirId, ItemKind as HirItemKind, Node};
use rustc_lint::{EarlyContext, EarlyLintPass, LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};
use rustc_session::{declare_lint, declare_lint_pass, impl_lint_pass};
use rustc_span::sym;

use crate::is_key_material_name;
use crate::is_sensitive_cmp_fn;
use crate::no_debug_on_signing_key_types::attrs_derive_trait;

declare_lint! {
    /// ### What it does
    ///
    /// Flags non-constant-time equality on auth/crypto surfaces:
    /// - `==` between byte-ish values in `verify` / `auth` / `mac` / `sig` / etc. functions
    /// - `#[derive(PartialEq)]` on key-material type names
    ///
    /// ### Why is this bad?
    ///
    /// Ordinary equality can short-circuit and leak timing information about secrets.
    /// Prefer `subtle::ConstantTimeEq` (or an equivalent CT compare).
    ///
    /// ### Example
    ///
    /// ```rust,ignore
    /// fn verify_mac(a: &[u8], b: &[u8]) -> bool {
    ///     a == b
    /// }
    /// ```
    ///
    /// Use instead: `bool::from(a.ct_eq(b))` via `subtle::ConstantTimeEq`.
    pub PREFER_SUBTLE_CT_EQ,
    Warn,
    "prefer constant-time equality for secret/auth byte comparisons"
}

// Early: PartialEq derive on key-material names.
declare_lint_pass!(PreferSubtleCtEqEarly => [PREFER_SUBTLE_CT_EQ]);

// Late: == on byte-ish values in sensitive functions.
pub struct PreferSubtleCtEqLate;
impl_lint_pass!(PreferSubtleCtEqLate => [PREFER_SUBTLE_CT_EQ]);

pub fn register(lint_store: &mut rustc_lint::LintStore) {
    lint_store.register_lints(&[PREFER_SUBTLE_CT_EQ]);
    // Pre-expansion: derive attrs are gone by early/late passes.
    lint_store.register_pre_expansion_pass(|| Box::new(PreferSubtleCtEqEarly));
    lint_store.register_late_pass(|_| Box::new(PreferSubtleCtEqLate));
}

impl EarlyLintPass for PreferSubtleCtEqEarly {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &AstItem) {
        let ident = match &item.kind {
            AstItemKind::Struct(ident, ..)
            | AstItemKind::Enum(ident, ..)
            | AstItemKind::Union(ident, ..) => ident,
            _ => return,
        };
        if !is_key_material_name(ident.as_str()) {
            return;
        }
        if !attrs_derive_trait(&item.attrs, "PartialEq") {
            return;
        }
        span_lint_and_help(
            cx,
            PREFER_SUBTLE_CT_EQ,
            ident.span,
            format!(
                "deriving `PartialEq` on key-material type `{}` invites non-constant-time secret compares",
                ident.as_str()
            ),
            None,
            "omit `PartialEq`, or compare via `subtle::ConstantTimeEq` in dedicated helpers",
        );
    }
}

impl<'tcx> LateLintPass<'tcx> for PreferSubtleCtEqLate {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let ExprKind::Binary(op, lhs, rhs) = expr.kind else {
            return;
        };
        if op.node != BinOpKind::Eq && op.node != BinOpKind::Ne {
            return;
        }

        let Some(fn_name) = enclosing_fn_name(cx, expr.hir_id) else {
            return;
        };
        if !is_sensitive_cmp_fn(&fn_name) {
            return;
        }

        let lhs_ty = cx.typeck_results().expr_ty(lhs).peel_refs();
        let rhs_ty = cx.typeck_results().expr_ty(rhs).peel_refs();
        if !(is_byteish(cx, lhs_ty) && is_byteish(cx, rhs_ty)) {
            return;
        }

        let help = if op.node == BinOpKind::Ne {
            "use `!bool::from(left.ct_eq(right))` (crate `subtle`) instead of `!=`"
        } else {
            "use `bool::from(left.ct_eq(right))` (crate `subtle`) instead of `==`"
        };
        span_lint_and_help(
            cx,
            PREFER_SUBTLE_CT_EQ,
            expr.span,
            format!(
                "byte equality in `{fn_name}` may not be constant-time; prefer `subtle::ConstantTimeEq`"
            ),
            None,
            help,
        );
    }
}

fn enclosing_fn_name(cx: &LateContext<'_>, hir_id: HirId) -> Option<String> {
    for (_id, node) in cx.tcx.hir_parent_iter(hir_id) {
        match node {
            Node::Item(item) => {
                if let HirItemKind::Fn { ident, .. } = item.kind {
                    return Some(ident.as_str().to_owned());
                }
            }
            Node::ImplItem(ii) => {
                if matches!(ii.kind, rustc_hir::ImplItemKind::Fn(..)) {
                    return Some(ii.ident.as_str().to_owned());
                }
            }
            Node::TraitItem(ti) => {
                if matches!(ti.kind, rustc_hir::TraitItemKind::Fn(..)) {
                    return Some(ti.ident.as_str().to_owned());
                }
            }
            Node::Expr(expr) => {
                if matches!(expr.kind, ExprKind::Closure(_)) {
                    continue;
                }
            }
            _ => {}
        }
    }
    None
}

fn is_byteish<'tcx>(cx: &LateContext<'tcx>, ty: Ty<'tcx>) -> bool {
    // Prefer buffers (slices/arrays/Vec), not bare `u8` (flag bit tests).
    let ty = ty.peel_refs();
    match *ty.kind() {
        ty::Slice(inner) => is_u8(inner),
        ty::Array(inner, _) => is_u8(inner),
        ty::Adt(adt, args) => {
            if cx.tcx.is_diagnostic_item(sym::Vec, adt.did()) {
                return args.types().next().is_some_and(is_u8);
            }
            let name = cx.tcx.item_name(adt.did());
            let s = name.as_str();
            s.ends_with("Bytes") || s == "ByteArray" || s == "Signature" || s == "Mac"
        }
        _ => false,
    }
}

fn is_u8(ty: Ty<'_>) -> bool {
    matches!(*ty.kind(), ty::Uint(ty::UintTy::U8))
}
