# lopdf 0.46.0 (MIT)

Upstream package SHA-256: `d43f64cb8c0d673ff84245a6de9e0172c0cc05dc5dd9d263660b5b4a02b6c4ae`.

Local copy of the crates.io library sources for the desktop PDF reader. The
application needs one cumulative limit across decoding performed inside the
reader and during subsequent content and font processing. Upstream 0.46.0
exposes only per-call limits. The local changes are in `stream_budget.rs`,
`lib.rs`, `object.rs` and `parser_aux.rs`. The reader grammar is upstream's.
Upstream examples, fixtures, test targets and development dependencies are
omitted. See LICENSE for the upstream license.
