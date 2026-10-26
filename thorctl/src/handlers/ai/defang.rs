//! Defangs IOCs in generated reports for safe transit and sharing.
//!
//! The implementation lives in [`thorpal::scrub`](thorpal::defang) so the
//! report `--defang` flag and the retry layer's content-filter request
//! scrubbing share one transform (and its inverse): the `--defang` flag
//! rewrites the copy written to the operator's path while the session store's
//! `report.md` stays pristine, and the report ladder refangs defanged model
//! replies back to pristine form with the same rules.

pub use thorpal::{defang, refang};
