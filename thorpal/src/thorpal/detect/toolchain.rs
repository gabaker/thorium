//! One-shot toolchain fingerprinting over the program-wide listings.
//!
//! A statically linked runtime ships hundreds of benign "findings": the Go and
//! Rust standard libraries carry codec tables, timing reads, file-deletion
//! calls and network vocabulary; MFC GUI apps import every user-activity API;
//! the MSVC CRT calls the debugger checks. Naming the toolchain once lets the
//! detections and the forum seeder judge all of that noise with one rule
//! instead of re-deriving it per hit. Detection is deliberately conservative:
//! two independent markers are required before a toolchain is named, so a
//! single stray string can never claim one.

use ghidra_cli::models::{Function, StringData, Symbol};

/// The compiler/runtime a binary statically links, when one is recognizable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchain {
    /// A Rust binary (rustc paths, panic strings, mangled core/std symbols).
    Rust,
    /// A Go binary (runtime.* symbols, go1. version tag, module paths).
    Go,
    /// A plain MSVC/C++ binary (CRT support-function markers).
    Msvc,
    /// An MFC application (afx vocabulary, mfc DLL references).
    Mfc,
    /// A Delphi/Borland binary (unit-qualified names, Borland stub text).
    Delphi,
}

impl Toolchain {
    /// The analyst-facing name used in notes and the seed rollup post.
    pub fn label(self) -> &'static str {
        // keep the labels short; they land in forum post titles and notes
        match self {
            Toolchain::Rust => "Rust",
            Toolchain::Go => "Go",
            Toolchain::Msvc => "MSVC/CRT",
            Toolchain::Mfc => "MFC",
            Toolchain::Delphi => "Delphi",
        }
    }
}

/// Function-name markers that identify a Go runtime.
const GO_FUNCTION_MARKERS: &[&str] = &["runtime.", "runtime_", "gcwritebarrier", "type.."];

/// String markers that identify a Go build.
const GO_STRING_MARKERS: &[&str] = &["go:buildid", "go1.", "/go/pkg/mod/", "/goroot/", "go build"];

/// Function-name markers that identify a Rust runtime.
const RUST_FUNCTION_MARKERS: &[&str] = &[
    "_zn4core",
    "_zn3std",
    "_zn5alloc",
    "core::",
    "std::",
    "alloc::",
    "__rust_",
    "rust_panic",
    "rust_eh",
    "rust_begin_unwind",
];

/// String markers that identify a Rust build.
const RUST_STRING_MARKERS: &[&str] = &[
    "/rustc/",
    "/cargo/registry/",
    "rust_backtrace",
    "called `option::unwrap()`",
    "attempt to add with overflow",
    "library/std/src/",
];

/// String markers that identify an MFC application.
const MFC_STRING_MARKERS: &[&str] = &["mfc42", "mfc71", "mfc80", "mfc140", "afxwin", "cwinapp"];

/// Function-name markers that identify an MFC application.
const MFC_FUNCTION_MARKERS: &[&str] = &["afx", "cwinapp", "cwnd::", "cdialog"];

/// String markers that identify a Delphi/Borland build.
const DELPHI_STRING_MARKERS: &[&str] = &[
    "this program must be run under win32",
    "borland",
    "embarcadero",
    "delphi",
];

/// Function-name markers that identify a Delphi/Borland build.
const DELPHI_FUNCTION_MARKERS: &[&str] = &["@system@", "system::", "sysutils::", "@sysutils@"];

/// Fingerprint the toolchain from the program-wide listings, if any.
///
/// Counts independent marker classes per toolchain (function names and strings
/// count separately, one point each) and names a toolchain only at two or more
/// points, preferring the most specific runtime on a tie. Purely static over
/// data the context already fetched — no extra bridge calls.
///
/// # Arguments
///
/// * `functions` - Every function in the program
/// * `imports` - The imported symbols
/// * `strings` - The defined strings
pub fn detect(
    functions: &[Function],
    imports: &[Symbol],
    strings: &[StringData],
) -> Option<Toolchain> {
    // flatten the listings onto the name-based core, which carries the logic
    detect_from_names(
        functions
            .iter()
            .map(|f| f.name.as_str())
            .chain(imports.iter().map(|s| s.name.as_str())),
        strings.iter().map(|s| s.value.as_str()),
    )
}

/// The name-based fingerprint core behind [`detect`].
///
/// Split out so the scoring logic is testable without constructing full
/// bridge models.
///
/// # Arguments
///
/// * `names` - Every function and import name in the program
/// * `values` - Every defined string value in the program
fn detect_from_names<'a>(
    names: impl Iterator<Item = &'a str>,
    values: impl Iterator<Item = &'a str>,
) -> Option<Toolchain> {
    // lowercase the listings once so every marker test is case-insensitive;
    // string values are truncated so a huge embedded blob is never duplicated
    // (every marker is short and sits near the front of real tool strings)
    let fn_names: Vec<String> = names.map(str::to_ascii_lowercase).collect();
    let string_values: Vec<String> = values
        .map(|value| {
            let end = value
                .char_indices()
                .nth(512)
                .map_or(value.len(), |(at, _)| at);
            value[..end].to_ascii_lowercase()
        })
        .collect();
    // one point per independent marker class that fires for a toolchain
    let score = |fn_markers: &[&str], string_markers: &[&str]| -> u32 {
        let mut points = 0;
        // any function (or import) name carrying a marker is one point
        if fn_names
            .iter()
            .any(|name| fn_markers.iter().any(|marker| name.contains(marker)))
        {
            points += 1;
        }
        // any defined string carrying a marker is one point
        if string_values
            .iter()
            .any(|value| string_markers.iter().any(|marker| value.contains(marker)))
        {
            points += 1;
        }
        points
    };
    // MSVC is special-cased: its signal is several CRT support functions, so it
    // earns both points only when two distinct marker functions exist
    let msvc_points = {
        let crt = fn_names
            .iter()
            .filter(|name| super::detections::is_library_function(name))
            .count();
        match crt {
            0 => 0,
            1 => 1,
            _ => 2,
        }
    };
    // score every candidate, most specific runtimes first for the tie-break
    let candidates = [
        (Toolchain::Go, score(GO_FUNCTION_MARKERS, GO_STRING_MARKERS)),
        (
            Toolchain::Rust,
            score(RUST_FUNCTION_MARKERS, RUST_STRING_MARKERS),
        ),
        (
            Toolchain::Delphi,
            score(DELPHI_FUNCTION_MARKERS, DELPHI_STRING_MARKERS),
        ),
        (
            Toolchain::Mfc,
            score(MFC_FUNCTION_MARKERS, MFC_STRING_MARKERS),
        ),
        (Toolchain::Msvc, msvc_points),
    ];
    // require two independent points; a Windows Rust/Go/MFC binary also links
    // the CRT, so ties go to the FIRST (most specific) candidate — the best
    // score is found first, then the earliest candidate holding it wins
    let best = candidates.iter().map(|(_, points)| *points).max()?;
    if best < 2 {
        return None;
    }
    candidates
        .into_iter()
        .find(|(_, points)| *points == best)
        .map(|(toolchain, _)| toolchain)
}

/// Whether `name` is runtime/stdlib code of the fingerprinted toolchain.
///
/// Complements [`super::detections::is_library_function`] (the MSVC marker
/// set): Go treats any package-qualified name outside `main.` as stdlib/
/// runtime; Rust matches its mangled and demangled core/std/alloc paths plus
/// the unwinder; MFC and Delphi match their class/unit vocabularies. The main
/// package of a Go binary and unmangled application code never match, so a
/// sample's own logic is never written off as runtime.
///
/// # Arguments
///
/// * `toolchain` - The fingerprinted toolchain
/// * `name` - The containing function's Ghidra name
pub fn is_toolchain_runtime(toolchain: Toolchain, name: &str) -> bool {
    // lowercase once so every marker test is case-insensitive
    let n = name.to_ascii_lowercase();
    match toolchain {
        Toolchain::Go => {
            // only the KNOWN stdlib/runtime namespaces count as runtime: the
            // sample's own packages (main.*, github.com/..., any third-party
            // module path) must stay loud — a Go stealer's c2/exfil packages
            // are its own logic, not noise
            const GO_STDLIB_PREFIXES: &[&str] = &[
                "runtime.",
                "runtime_",
                "sync.",
                "sync/",
                "syscall.",
                "syscall/",
                "fmt.",
                "os.",
                "os/",
                "net.",
                "net/",
                "time.",
                "reflect.",
                "strconv.",
                "unicode.",
                "unicode/",
                "strings.",
                "bytes.",
                "bufio.",
                "io.",
                "io/",
                "sort.",
                "errors.",
                "context.",
                "math.",
                "math/",
                "hash.",
                "hash/",
                "internal/",
                "crypto/",
                "encoding/",
                "compress/",
                "container/",
                "database/",
                "debug/",
                "regexp.",
                "regexp/",
                "path.",
                "path/",
                "mime.",
                "mime/",
                "log.",
                "log/",
                "text/",
                "archive/",
                "vendor/",
                "golang.org/x/",
                "type..",
                "gcwritebarrier",
            ];
            GO_STDLIB_PREFIXES
                .iter()
                .any(|prefix| n.starts_with(prefix))
        }
        Toolchain::Rust => RUST_FUNCTION_MARKERS
            .iter()
            .any(|marker| n.contains(marker)),
        Toolchain::Msvc => super::detections::is_library_function(&n),
        Toolchain::Mfc => {
            // MFC apps are MSVC apps too, so the CRT markers also count
            MFC_FUNCTION_MARKERS.iter().any(|marker| n.contains(marker))
                || super::detections::is_library_function(&n)
        }
        Toolchain::Delphi => DELPHI_FUNCTION_MARKERS
            .iter()
            .any(|marker| n.contains(marker)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the name-based core over plain string slices.
    ///
    /// # Arguments
    ///
    /// * `names` - The function/import names to test with
    /// * `values` - The string values to test with
    fn detect_strs(names: &[&str], values: &[&str]) -> Option<Toolchain> {
        detect_from_names(names.iter().copied(), values.iter().copied())
    }

    /// Two independent marker classes name a toolchain; one does not.
    #[test]
    fn detect_requires_two_independent_markers() {
        // a Go function marker plus a Go build string names Go
        let go = detect_strs(&["runtime.morestack", "main.run"], &["go1.21.3"]);
        assert_eq!(go, Some(Toolchain::Go));
        // the string alone is not enough to claim a toolchain
        assert_eq!(detect_strs(&["FUN_00401000"], &["go1.21.3"]), None);
        // an empty program names nothing
        assert_eq!(detect_strs(&[], &[]), None);
    }

    /// Rust fingerprints fire on panic strings plus mangled symbols.
    #[test]
    fn detect_names_rust() {
        let rust = detect_strs(
            &["_ZN4core3fmt5write17h1234567890abcdefE"],
            &["/rustc/abcdef/library/core/src/fmt/mod.rs"],
        );
        assert_eq!(rust, Some(Toolchain::Rust));
    }

    /// Go's own main package is never classified as runtime code.
    #[test]
    fn go_main_package_is_not_runtime() {
        // the sample's own logic stays loud
        assert!(!is_toolchain_runtime(Toolchain::Go, "main.decryptConfig"));
        // the runtime and stdlib are recognized
        assert!(is_toolchain_runtime(Toolchain::Go, "runtime.morestack"));
        assert!(is_toolchain_runtime(
            Toolchain::Go,
            "crypto/aes.encryptBlockGo"
        ));
        assert!(is_toolchain_runtime(Toolchain::Go, "net/http.(*Client).do"));
    }

    /// Rust runtime markers match mangled and demangled stdlib names only.
    #[test]
    fn rust_runtime_matches_stdlib_names() {
        assert!(is_toolchain_runtime(Toolchain::Rust, "core::fmt::write"));
        assert!(is_toolchain_runtime(
            Toolchain::Rust,
            "_ZN3std2io5Write9write_fmt17h0f1e2d3c4b5a6978E"
        ));
        // an application function never matches
        assert!(!is_toolchain_runtime(Toolchain::Rust, "decrypt_config"));
        assert!(!is_toolchain_runtime(Toolchain::Rust, "FUN_00401000"));
    }
}
