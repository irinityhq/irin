//! Per-round seat divergence (N02) — 2-component PCA over seat embeddings.
//!
//! After each round's convergence judge runs, the streaming engine embeds each
//! seat's response and projects the 384-dim vectors down to 2D so the War Room
//! `LiveAnalytics` panel can draw a scatter of where the seats sit relative to
//! one another.
//!
//! UMAP has no mature pure-Rust crate and the heavy ML crates (linfa/ndarray)
//! are out of scope per the Phase 9 contract, so this is an honest hand-rolled
//! **PCA** (power iteration for the top two principal components). The
//! `method` field on the wire says `"pca"` truthfully — never `"umap"`.
//!
//! Deterministic given the input vectors: power iteration is seeded from a
//! fixed deterministic vector, so the same embeddings always project the same
//! way (up to a sign, which we canonicalize).

use crate::warroom::embeddings;

/// A single seat's 2D projection. `seat` is the seat name; `x`/`y` are the
/// principal-component coordinates.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DivergencePoint {
    pub seat: String,
    pub x: f64,
    pub y: f64,
}

/// Embed each seat's text and project to 2D via PCA.
///
/// Returns `None` when embeddings are unavailable (fastembed init/encode
/// failed) or there is nothing meaningful to project (< 2 seats) — the caller
/// then omits the `round_divergence` event entirely, which the UI tolerates.
///
/// `labels` and `texts` must be the same length and aligned.
pub fn project_seats(labels: &[String], texts: &[String]) -> Option<Vec<DivergencePoint>> {
    if labels.len() != texts.len() || labels.len() < 2 {
        return None;
    }
    let owned: Vec<String> = texts.to_vec();
    let vectors = embeddings::embed_texts_public(&owned).ok()?;
    if vectors.len() != labels.len() {
        return None;
    }
    let coords = pca_2d(&vectors)?;
    Some(
        labels
            .iter()
            .zip(coords)
            .map(|(seat, (x, y))| DivergencePoint {
                seat: seat.clone(),
                x,
                y,
            })
            .collect(),
    )
}

/// Project a set of equal-length vectors onto their top two principal
/// components. Returns one `(x, y)` per input row, or `None` if the input is
/// degenerate (empty / ragged / single row).
///
/// Hand-rolled: mean-center, then power-iterate the covariance action to find
/// PC1, deflate, power-iterate again for PC2. No covariance matrix is
/// materialized — we apply `Cov · v = (1/n) Σ_i x_i (x_i · v)` directly so the
/// cost stays O(rows · dims) per iteration.
pub fn pca_2d(vectors: &[Vec<f32>]) -> Option<Vec<(f64, f64)>> {
    let n = vectors.len();
    if n < 2 {
        return None;
    }
    let dim = vectors[0].len();
    if dim == 0 || vectors.iter().any(|v| v.len() != dim) {
        return None;
    }

    // Mean-center into f64 rows.
    let mut mean = vec![0.0f64; dim];
    for v in vectors {
        for (m, &x) in mean.iter_mut().zip(v.iter()) {
            *m += x as f64;
        }
    }
    for m in mean.iter_mut() {
        *m /= n as f64;
    }
    let rows: Vec<Vec<f64>> = vectors
        .iter()
        .map(|v| {
            v.iter()
                .zip(mean.iter())
                .map(|(&x, &m)| x as f64 - m)
                .collect()
        })
        .collect();

    let pc1 = power_iteration(&rows, dim, None)?;
    let pc2 = power_iteration(&rows, dim, Some(&pc1)).unwrap_or_else(|| zero_basis(dim, &pc1));

    // Project each centered row onto the two components.
    let coords: Vec<(f64, f64)> = rows.iter().map(|r| (dot(r, &pc1), dot(r, &pc2))).collect();

    Some(canonicalize_sign(coords))
}

/// Apply the (un-normalized) covariance operator to `v`:
/// `out = Σ_i row_i (row_i · v)`. Scaling by `1/n` is irrelevant to the
/// dominant eigenvector direction, so it is skipped.
fn cov_apply(rows: &[Vec<f64>], v: &[f64]) -> Vec<f64> {
    let dim = v.len();
    let mut out = vec![0.0f64; dim];
    for r in rows {
        let proj = dot(r, v);
        for (o, &x) in out.iter_mut().zip(r.iter()) {
            *o += x * proj;
        }
    }
    out
}

/// Power iteration for the top eigenvector of the covariance operator. When
/// `deflate` is provided, the iterate is re-orthogonalized against it each step
/// so the result is the second component.
fn power_iteration(rows: &[Vec<f64>], dim: usize, deflate: Option<&[f64]>) -> Option<Vec<f64>> {
    // Deterministic seed: a fixed pseudo-random-ish but reproducible vector so
    // the same embeddings always yield the same projection.
    let mut v: Vec<f64> = (0..dim)
        .map(|i| (((i * 2654435761).wrapping_add(1) % 1000) as f64 / 1000.0) - 0.5)
        .collect();
    if let Some(d) = deflate {
        orthogonalize(&mut v, d);
    }
    normalize(&mut v)?;

    for _ in 0..100 {
        let mut next = cov_apply(rows, &v);
        if let Some(d) = deflate {
            orthogonalize(&mut next, d);
        }
        // Collapsed to zero (`None`) — no variance left in this direction.
        normalize(&mut next)?;
        // Convergence: cosine with previous iterate ~ 1.
        let cos = dot(&next, &v).abs();
        v = next;
        if cos > 1.0 - 1e-9 {
            break;
        }
    }
    Some(v)
}

/// A fallback orthonormal-ish basis vector when PC2 has no variance (e.g. all
/// rows collinear). Picks the first axis orthogonal to PC1.
fn zero_basis(dim: usize, pc1: &[f64]) -> Vec<f64> {
    for axis in 0..dim {
        let mut e = vec![0.0f64; dim];
        e[axis] = 1.0;
        orthogonalize(&mut e, pc1);
        if normalize(&mut e).is_some() {
            return e;
        }
    }
    vec![0.0f64; dim]
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn normalize(v: &mut [f64]) -> Option<()> {
    let norm = dot(v, v).sqrt();
    if norm < 1e-12 {
        return None;
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    Some(())
}

/// Remove the component of `v` along the (unit) vector `basis`.
fn orthogonalize(v: &mut [f64], basis: &[f64]) {
    let proj = dot(v, basis);
    for (x, &b) in v.iter_mut().zip(basis.iter()) {
        *x -= proj * b;
    }
}

/// Canonicalize the sign of each axis so the projection is reproducible
/// regardless of which eigenvector sign power iteration converged to: flip an
/// axis if its largest-magnitude coordinate is negative.
fn canonicalize_sign(mut coords: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    // X axis
    let flip_x = coords
        .iter()
        .map(|c| c.0)
        .fold(0.0f64, |acc, x| if x.abs() > acc.abs() { x } else { acc })
        < 0.0;
    let flip_y = coords
        .iter()
        .map(|c| c.1)
        .fold(0.0f64, |acc, y| if y.abs() > acc.abs() { y } else { acc })
        < 0.0;
    if flip_x || flip_y {
        for c in coords.iter_mut() {
            if flip_x {
                c.0 = -c.0;
            }
            if flip_y {
                c.1 = -c.1;
            }
        }
    }
    // Round to 6 dp — keeps the wire payload small and deterministic.
    for c in coords.iter_mut() {
        c.0 = (c.0 * 1e6).round() / 1e6;
        c.1 = (c.1 * 1e6).round() / 1e6;
    }
    coords
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build two well-separated clusters of vectors along a known axis. PCA's
    /// first component should capture that axis, so the projected x-coords
    /// separate the clusters (the dominant variance ends up on PC1).
    #[test]
    fn pca_2d_orders_variance_on_first_component() {
        // dim = 4; variance is largest along axis 0, then axis 1, ~0 elsewhere.
        let vectors = vec![
            vec![10.0, 1.0, 0.0, 0.0],
            vec![-10.0, -1.0, 0.0, 0.0],
            vec![9.0, -1.0, 0.0, 0.0],
            vec![-9.0, 1.0, 0.0, 0.0],
        ];
        let coords = pca_2d(&vectors).expect("pca should produce coords");
        assert_eq!(coords.len(), 4);

        // Spread along PC1 (x) must dominate spread along PC2 (y) because axis 0
        // carries the most variance.
        let span = |sel: fn(&(f64, f64)) -> f64| {
            let xs: Vec<f64> = coords.iter().map(sel).collect();
            let max = xs.iter().cloned().fold(f64::MIN, f64::max);
            let min = xs.iter().cloned().fold(f64::MAX, f64::min);
            max - min
        };
        let x_span = span(|c| c.0);
        let y_span = span(|c| c.1);
        assert!(
            x_span > y_span,
            "PC1 span ({x_span}) should exceed PC2 span ({y_span})"
        );
        // The two clusters (rows 0,2 vs 1,3) must land on opposite x signs.
        assert!(
            coords[0].0 * coords[1].0 < 0.0,
            "clusters not separated on x"
        );
    }

    #[test]
    fn pca_2d_is_deterministic() {
        let vectors = vec![
            vec![1.0, 2.0, 3.0],
            vec![4.0, 1.0, 0.0],
            vec![-2.0, 5.0, 1.0],
        ];
        let a = pca_2d(&vectors).unwrap();
        let b = pca_2d(&vectors).unwrap();
        assert_eq!(a, b, "PCA must be deterministic given identical input");
    }

    #[test]
    fn pca_2d_rejects_degenerate_input() {
        assert!(pca_2d(&[]).is_none());
        assert!(pca_2d(&[vec![1.0, 2.0]]).is_none(), "single row");
        assert!(
            pca_2d(&[vec![1.0, 2.0], vec![3.0]]).is_none(),
            "ragged rows"
        );
        assert!(pca_2d(&[vec![], vec![]]).is_none(), "zero-dim rows");
    }

    #[test]
    fn pca_2d_handles_collinear_rows_without_panicking() {
        // All rows on a line through the origin → PC2 has no variance. Must not
        // panic; PC2 coords should all be ~0.
        let vectors = vec![
            vec![1.0, 1.0, 1.0],
            vec![2.0, 2.0, 2.0],
            vec![3.0, 3.0, 3.0],
        ];
        let coords = pca_2d(&vectors).expect("collinear still projects");
        assert_eq!(coords.len(), 3);
        for c in &coords {
            assert!(c.1.abs() < 1e-6, "PC2 should be ~0 for collinear input");
        }
    }

    #[test]
    fn project_seats_rejects_misaligned_or_too_few() {
        // Mismatched lengths → None (no embedding attempt).
        assert!(
            project_seats(&["a".into()], &["x".into(), "y".into()]).is_none(),
            "ragged labels/texts"
        );
        // Single seat → None (nothing to diverge from).
        assert!(
            project_seats(&["a".into()], &["x".into()]).is_none(),
            "single seat"
        );
    }
}
