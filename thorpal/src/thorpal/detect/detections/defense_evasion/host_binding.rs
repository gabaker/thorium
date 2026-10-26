//! Host binding — machine identity the program is keyed to (**T1480.001**,
//! with the enumeration half reported as **T1082**).
//!
//! A program that reads the machine's MAC address, volume serial, machine GUID,
//! computer name, or account name and then *compares the result against a value
//! it already carries* is not gathering information — it is checking whether it
//! is on the intended host. That is an execution guardrail: on any other machine
//! the interesting code path never runs, so a sandbox detonation shows nothing
//! and a static reader who misses the check concludes the sample is inert.
//!
//! The recovered value is also, on its own, one of the most pivotable artifacts
//! a static pass can produce: it names a specific machine.
//!
//! # What is measured
//!
//! Four independent angles, all of which report the *enumerator* alongside the
//! value, because a bare literal is not actionable:
//!
//! 1. **Shaped literals** — MAC, GUID, and volume-serial renderings recovered
//!    from the string table via [`crate::thorpal::detect::indicators::find_host_identifiers`],
//!    attributed to the function that references them.
//! 2. **Name candidates** — computer, domain, and account names have no shape at
//!    all, so they are only recovered from *inside* a function that calls a name
//!    enumerator. The enumeration is what makes the literal a candidate.
//! 3. **Binary identifiers** — six-byte MAC and sixteen-byte GUID values stored
//!    as bytes rather than text, searched **only** in the buffers that
//!    enumerating functions reference.
//! 4. **Pooled literals** — the same rendered shapes as (1), but read out of the
//!    raw bytes of the data blocks, so that a value in a concatenated literal
//!    pool the string finder never split into strings is still recovered.
//!
//! # Why the enumerator matters so much here
//!
//! Every one of these shapes is cheap to match and expensive to be wrong about.
//! A six-byte pattern occurs constantly in any binary; an `XXXX-XXXX` hex run is
//! also a part number; a short identifier is also a registry subkey. What
//! separates an environmental key from a coincidence is not the value — it is
//! that the program *asks the operating system for the same class of value* and
//! then compares. So a literal with no enumerator anywhere near it is reported
//! at [`low`] and [`EvidenceKind::ContextOnly`]; a literal inside an enumerating
//! function is [`medium`]; and only an observed comparison feeding a branch
//! earns [`EvidenceKind::ControlFlowGated`], which is what lets `validate.rs`
//! confirm it. Nothing here is ever *emitted* at `Confidence::Fact` — a shape
//! match is not a fact about the machine it appears to name. A hit can still
//! reach the top of the scale afterwards, but only on evidence this module does
//! not have: a branch `validate.rs` observed, or capa independently asserting the
//! same capability in the same function.
//!
//! # Statically linked runtimes
//!
//! A Go or Rust binary reaches adapter enumeration through its own runtime with
//! no matching import, so the import catalog below is paired with a table of
//! runtime symbol markers matched through
//! [`crate::thorpal::detect::indicators::match_runtime_marker`]. Without it, this detection is
//! blind on exactly the binaries that are hardest to read by other means.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};

use crate::thorpal::detect::context::{AnalysisContext, Section, fmt_addr, parse_addr};
use crate::thorpal::detect::decode::buffers::referencing_function;
use crate::thorpal::detect::detections::{
    ArtifactClass, AttackTechnique, Detection, DetectionHit, EvidenceKind, HitArtifact,
    is_library_function, low, medium, normalize,
};
use crate::thorpal::detect::indicators::{
    HostIdKind, find_blob_host_identifiers, find_host_identifiers, is_degenerate_mac,
    match_runtime_marker, plausible_host_name,
};

use super::{T1082, T1480_001};

/// The Host Binding detection (ATT&CK T1480.001, with T1082 for the enumeration).
pub struct HostBinding;

/// How far from a referenced buffer base an identifier may sit and still be
/// attributed to the referencing function, in bytes.
///
/// Matches the window the other data-derived detections use, so a value carved
/// out of the middle of a config blob is attributed the same way everywhere.
const DATA_REF_WINDOW: u64 = 4096;

/// The same window, for a value recovered from a concatenated literal pool.
///
/// The wide window above assumes a referenced buffer with a value somewhere inside
/// it, which is what a config blob is. A literal pool is the opposite shape: it is
/// thousands of unrelated strings packed end to end, each referenced at its own
/// address, so four kilobytes below a value spans hundreds of literals that have
/// nothing to do with it and the nearest reference is a coin flip. Kept short so the
/// fallback either finds the reference that genuinely belongs to the value or finds
/// nothing — and finding nothing is the honest answer, which [`build_identity_hit`]
/// already scores at [`low`].
const BLOB_REF_WINDOW: u64 = 256;

/// How many bytes of a referenced buffer the binary scan reads.
///
/// The scan is deliberately narrow: it looks at what an enumerating function
/// points at, not at the whole image. A structure holding an adapter address or
/// a machine GUID sits within a few hundred bytes of the pointer that reaches it.
const BINARY_SCAN_WINDOW: usize = 512;

/// How many bytes either side of a binary MAC candidate are read to decide whether
/// it is really a slice of a table.
///
/// Wide enough to hold several elements of any plausible stride — four elements of
/// the widest [`crate::thorpal::detect::decode::recover`] tests for — and narrow enough that a single
/// adapter structure next to unrelated data is judged on its own bytes.
const MAC_CONTEXT_RADIUS: usize = 24;

/// How long a run of text a binary MAC candidate may touch before it is read as a
/// slice of that text rather than as an address.
///
/// Six printable bytes in a row are unremarkable — a real address hits the printable
/// range about a third of the time per octet — so the bar is on the run the candidate
/// belongs to, not on the candidate. Eight is past the length any address can reach on
/// its own (six bytes) and short enough to catch the string fragments that produce
/// these: a program name, a flag word, a fragment of a usage banner.
const MAC_TEXT_RUN: usize = 8;

/// How much of a binary MAC candidate must lie inside a run of text before the run
/// explains it.
///
/// A candidate that merely abuts a string — a printable byte or two of overlap with a
/// long banner next door — is not a slice of it. Half the candidate is the bar, and it
/// is safe to set that low because C strings are terminated: a real datum stored after
/// one is separated from it by the NUL, which breaks the run, so the only candidates
/// that reach into a long run of text are the ones cut out of it.
const MAC_TEXT_OVERLAP: usize = 3;

/// What share of a binary MAC candidate's neighbourhood must be printable ASCII or
/// NUL before the neighbourhood is read as a string table rather than as data.
///
/// [`in_text_run`] measures one run and needs it to be [`MAC_TEXT_RUN`] long, which a
/// table of short strings never is: `--help\0\0--sleep\0--delay\0` is six-, seven-,
/// and six-byte runs, so a window straddling any of them reads as an address with two
/// NULs in it — under the half-zeros bar and past every other test [`render_mac`] has.
/// The table is obvious a little further out, though, because a C string table is
/// nothing but printable bytes and terminators.
///
/// The bar is total — *every* byte in the neighbourhood — which is both where the
/// measurement puts it and the most conservative place it could go. Swept over the
/// sample binaries' data sections, of the 241,735 six-byte windows that clear
/// [`render_mac`] and [`in_int_table`], the density buckets are flat at two to five
/// thousand windows each until the last one:
///
/// | neighbourhood printable-or-NUL | windows | share |
/// |--------------------------------|---------|-------|
/// | 90–94%                         | 11,108  | 4.6%  |
/// | 95–99%                         | 7,222   | 3.0%  |
/// | 100%                           | 43,695  | 18.1% |
///
/// The sixfold spike in the last bucket is the string tables and nothing else, so the
/// knee is at the top and there is no reason to reach below it. What that costs a
/// genuine address is bounded by the candidate's own six bytes, which are inside the
/// window it is judged on: one byte of a stored address that is neither printable nor
/// NUL — a high byte, a control byte — puts the neighbourhood under 100% by itself and
/// makes the test unreachable. Six such bytes in a row land in range about six times in
/// a hundred, and those six must then be surrounded by nothing but text.
const MAC_STRING_TABLE_SHARE: usize = 100;

/// The widths a stored address is written at, in bytes.
///
/// Both are tried rather than one being chosen from the program's architecture,
/// because the choice makes no difference and the check is cheap: a word only counts
/// when it decodes to an address inside a loaded block, and a value that reaches the
/// blocks of a sixty-four-bit image does not fit in four bytes, while one read as
/// eight bytes of a thirty-two-bit image carries four high bytes of neighbouring data
/// that put it far outside them. The wrong width answers no on its own.
const POINTER_WIDTHS: [u64; 2] = [8, 4];

/// The most literal findings reported per identity class.
///
/// Per-class rather than global so a program carrying a hundred GUIDs cannot
/// crowd out its one MAC address. Truncation is always logged.
const MAX_PER_CLASS: usize = 24;

/// The most name candidates reported from any single enumerating function.
///
/// A function that reads the computer name and then formats it into a dozen log
/// messages references a dozen strings; the first few are enough to show the
/// analyst what is being compared.
const MAX_NAMES_PER_FUNCTION: usize = 8;

/// What a host-identity value describes.
///
/// A superset of [`HostIdKind`]: the shaped literals it covers, plus the
/// name classes that have no shape and are only ever recovered by proximity to
/// their enumerator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum IdentityClass {
    /// A network adapter's hardware address.
    MacAddress,
    /// A machine, volume, installation, or product GUID.
    Guid,
    /// A volume or disk serial number.
    VolumeSerial,
    /// A computer, NetBIOS, or domain name.
    MachineName,
    /// An account or user name.
    AccountName,
}

impl IdentityClass {
    /// The noun used for this class in the finding text.
    ///
    /// The three shaped classes defer to [`HostIdKind`] so the classifier and the
    /// detection can never describe the same value two different ways.
    fn label(self) -> &'static str {
        match self {
            Self::MacAddress => HostIdKind::MacAddress.label(),
            Self::Guid => HostIdKind::Guid.label(),
            Self::VolumeSerial => HostIdKind::VolumeSerial.label(),
            Self::MachineName => "computer or domain name",
            Self::AccountName => "account name",
        }
    }

    /// The class of the equivalent shaped literal.
    ///
    /// # Arguments
    ///
    /// * `kind` - The shape the indicator classifier matched
    fn from_shape(kind: HostIdKind) -> Self {
        match kind {
            HostIdKind::MacAddress => Self::MacAddress,
            HostIdKind::Guid => Self::Guid,
            HostIdKind::VolumeSerial => Self::VolumeSerial,
        }
    }

    /// How many bytes this class occupies when stored as raw data.
    ///
    /// Only the two classes [`scan_window`] searches for have a fixed width; the
    /// others are text, and are never found by the byte scan.
    fn byte_len(self) -> u64 {
        match self {
            Self::MacAddress => 6,
            Self::Guid => 16,
            // a name or serial is a string, so its width is whatever it is — no
            // caller reaches this, and one byte claims nothing beyond the value
            Self::VolumeSerial | Self::MachineName | Self::AccountName => 1,
        }
    }

    /// The artifact class this identity is inventoried under.
    fn artifact(self) -> ArtifactClass {
        // every identity class is the same kind of artifact: a value that names
        // the machine, which is what an analyst pivots on
        ArtifactClass::HostIdentifier
    }
}

/// One operating-system call that hands the program a piece of machine identity.
struct Enumerator {
    /// The [`normalize`]d import name to match.
    key: &'static str,
    /// What the call retrieves.
    class: IdentityClass,
    /// How the call is described in the finding text.
    label: &'static str,
}

/// The identity-enumeration APIs, across Windows and POSIX.
///
/// A platform fact table: it changes when an OS ships a new identity API, not
/// when a new sample appears. Keys are matched after [`normalize`], so the
/// ANSI/Wide suffix and the import decoration are already gone.
const ENUMERATORS: &[Enumerator] = &[
    // --- Windows: adapter hardware addresses ---
    Enumerator {
        key: "GetAdaptersAddresses",
        class: IdentityClass::MacAddress,
        label: "GetAdaptersAddresses (network adapter enumeration)",
    },
    Enumerator {
        key: "GetAdaptersInfo",
        class: IdentityClass::MacAddress,
        label: "GetAdaptersInfo (network adapter enumeration)",
    },
    Enumerator {
        key: "GetIfTable",
        class: IdentityClass::MacAddress,
        label: "GetIfTable (interface table enumeration)",
    },
    Enumerator {
        key: "GetIfTable2",
        class: IdentityClass::MacAddress,
        label: "GetIfTable2 (interface table enumeration)",
    },
    Enumerator {
        key: "SendARP",
        class: IdentityClass::MacAddress,
        label: "SendARP (resolves an address to adapter hardware)",
    },
    Enumerator {
        key: "Netbios",
        class: IdentityClass::MacAddress,
        label: "Netbios (NCBASTAT returns adapter hardware addresses)",
    },
    // --- Windows: volume and machine identity ---
    Enumerator {
        key: "GetVolumeInformation",
        class: IdentityClass::VolumeSerial,
        label: "GetVolumeInformation (volume serial number)",
    },
    Enumerator {
        key: "GetVolumeInformationByHandle",
        class: IdentityClass::VolumeSerial,
        label: "GetVolumeInformationByHandle (volume serial number)",
    },
    Enumerator {
        key: "NtQueryVolumeInformationFile",
        class: IdentityClass::VolumeSerial,
        label: "NtQueryVolumeInformationFile (volume serial number)",
    },
    Enumerator {
        key: "GetSystemFirmwareTable",
        class: IdentityClass::Guid,
        label: "GetSystemFirmwareTable (SMBIOS system UUID)",
    },
    Enumerator {
        key: "UuidCreateSequential",
        class: IdentityClass::MacAddress,
        label: "UuidCreateSequential (embeds the adapter hardware address)",
    },
    // --- Windows: machine and account names ---
    Enumerator {
        key: "GetComputerName",
        class: IdentityClass::MachineName,
        label: "GetComputerName",
    },
    Enumerator {
        key: "GetComputerNameEx",
        class: IdentityClass::MachineName,
        label: "GetComputerNameEx",
    },
    Enumerator {
        key: "NetWkstaGetInfo",
        class: IdentityClass::MachineName,
        label: "NetWkstaGetInfo (workstation and domain name)",
    },
    Enumerator {
        key: "DsGetDcName",
        class: IdentityClass::MachineName,
        label: "DsGetDcName (domain controller / domain name)",
    },
    Enumerator {
        key: "GetUserName",
        class: IdentityClass::AccountName,
        label: "GetUserName",
    },
    Enumerator {
        key: "GetUserNameEx",
        class: IdentityClass::AccountName,
        label: "GetUserNameEx",
    },
    Enumerator {
        key: "LookupAccountSid",
        class: IdentityClass::AccountName,
        label: "LookupAccountSid",
    },
    Enumerator {
        key: "NetUserGetInfo",
        class: IdentityClass::AccountName,
        label: "NetUserGetInfo",
    },
    // --- POSIX ---
    Enumerator {
        key: "getifaddrs",
        class: IdentityClass::MacAddress,
        label: "getifaddrs (interface enumeration)",
    },
    Enumerator {
        key: "if_nameindex",
        class: IdentityClass::MacAddress,
        label: "if_nameindex (interface enumeration)",
    },
    Enumerator {
        key: "gethostname",
        class: IdentityClass::MachineName,
        label: "gethostname",
    },
    Enumerator {
        key: "getdomainname",
        class: IdentityClass::MachineName,
        label: "getdomainname",
    },
    Enumerator {
        key: "uname",
        class: IdentityClass::MachineName,
        label: "uname (nodename)",
    },
    Enumerator {
        key: "getlogin",
        class: IdentityClass::AccountName,
        label: "getlogin",
    },
    Enumerator {
        key: "getlogin_r",
        class: IdentityClass::AccountName,
        label: "getlogin_r",
    },
    Enumerator {
        key: "getpwuid",
        class: IdentityClass::AccountName,
        label: "getpwuid (account record)",
    },
    Enumerator {
        key: "cuserid",
        class: IdentityClass::AccountName,
        label: "cuserid",
    },
];

/// One runtime symbol that betrays identity enumeration in a linked package.
struct RuntimeEnumerator {
    /// The lower-case substring matched against each Ghidra function name.
    marker: &'static str,
    /// What the package call retrieves.
    class: IdentityClass,
    /// How the package is described in the finding text.
    label: &'static str,
}

/// Identity-enumeration markers for statically linked Go and Rust binaries.
///
/// These binaries have no import table, so the enumeration is only visible in
/// the symbol names Ghidra recovers. Markers are kept package-qualified
/// (`net.interface`, `whoami::`, a length-prefixed `8hostname`) because the bare
/// words collide freely: a plain `hostname` matches any function that formats
/// one, and a plain `uuid` matches every generator.
const RUNTIME_ENUMERATORS: &[RuntimeEnumerator] = &[
    // --- Go standard library ---
    RuntimeEnumerator {
        marker: "net.interfaces",
        class: IdentityClass::MacAddress,
        label: "Go net.Interfaces",
    },
    RuntimeEnumerator {
        marker: "net.interfacebyname",
        class: IdentityClass::MacAddress,
        label: "Go net.InterfaceByName",
    },
    RuntimeEnumerator {
        marker: "net.interfaceaddrs",
        class: IdentityClass::MacAddress,
        label: "Go net.InterfaceAddrs",
    },
    RuntimeEnumerator {
        marker: "os.hostname",
        class: IdentityClass::MachineName,
        label: "Go os.Hostname",
    },
    RuntimeEnumerator {
        marker: "os/user.current",
        class: IdentityClass::AccountName,
        label: "Go os/user.Current",
    },
    RuntimeEnumerator {
        marker: "os/user.lookup",
        class: IdentityClass::AccountName,
        label: "Go os/user.Lookup",
    },
    // --- Rust crates (demangled `crate::` form and mangled `_ZN<len><crate>` form) ---
    RuntimeEnumerator {
        marker: "mac_address",
        class: IdentityClass::MacAddress,
        label: "Rust mac_address",
    },
    RuntimeEnumerator {
        marker: "get_if_addrs",
        class: IdentityClass::MacAddress,
        label: "Rust get_if_addrs",
    },
    RuntimeEnumerator {
        marker: "if_addrs::",
        class: IdentityClass::MacAddress,
        label: "Rust if-addrs",
    },
    RuntimeEnumerator {
        marker: "8if_addrs",
        class: IdentityClass::MacAddress,
        label: "Rust if-addrs",
    },
    RuntimeEnumerator {
        marker: "pnet_datalink",
        class: IdentityClass::MacAddress,
        label: "Rust pnet_datalink",
    },
    RuntimeEnumerator {
        marker: "machine_uid",
        class: IdentityClass::Guid,
        label: "Rust machine-uid",
    },
    RuntimeEnumerator {
        marker: "hostname::get",
        class: IdentityClass::MachineName,
        label: "Rust hostname::get",
    },
    RuntimeEnumerator {
        marker: "8hostname3get",
        class: IdentityClass::MachineName,
        label: "Rust hostname::get",
    },
    RuntimeEnumerator {
        marker: "gethostname::",
        class: IdentityClass::MachineName,
        label: "Rust gethostname",
    },
    RuntimeEnumerator {
        marker: "whoami::",
        class: IdentityClass::AccountName,
        label: "Rust whoami",
    },
    RuntimeEnumerator {
        marker: "6whoami",
        class: IdentityClass::AccountName,
        label: "Rust whoami",
    },
    // --- Managed runtimes compiled or embedded into a native image ---
    RuntimeEnumerator {
        marker: "networkinterface.getall",
        class: IdentityClass::MacAddress,
        label: ".NET NetworkInterface.GetAllNetworkInterfaces",
    },
    RuntimeEnumerator {
        marker: "java/net/networkinterface",
        class: IdentityClass::MacAddress,
        label: "Java NetworkInterface",
    },
];

/// A function observed asking the operating system for machine identity.
#[derive(Debug, Clone)]
struct EnumeratingFunction {
    /// The function's entry address.
    entry: u64,
    /// Every identity class it enumerates.
    classes: BTreeSet<IdentityClass>,
    /// How each enumerator is described, for the finding text.
    labels: BTreeSet<&'static str>,
    /// The call sites of the enumerators, when they were resolved from imports.
    sites: Vec<u64>,
}

impl EnumeratingFunction {
    /// The enumerator labels rendered as one phrase.
    fn describe(&self) -> String {
        // stable order, since the labels live in a sorted set
        self.labels.iter().copied().collect::<Vec<_>>().join(", ")
    }
}

#[async_trait::async_trait]
impl Detection for HostBinding {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "host_binding"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1480_001
    }

    /// Recover machine-identity values and tie each to the code that reads them.
    #[tracing::instrument(name = "HostBinding::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // find every function that asks the OS for machine identity — this is what
        // turns each of the shape-only recoveries below into evidence
        let enumerators = find_enumerators(ctx).await?;
        let mut hits = Vec::new();
        // report the enumeration itself (T1082), so the analyst sees the read even
        // when no stored value is recovered to compare it against
        hits.extend(enumeration_hits(ctx, &enumerators));
        // ...and the same read where stripping left only the runtime's own name table
        hits.extend(runtime_symbol_hits(ctx, &enumerators));
        // the two text passes search the same bytes by different routes, so they share
        // a cap tally and a set of claimed addresses: a value the string table already
        // yielded must not be reported a second time by the raw scan
        let mut counts: BTreeMap<IdentityClass, usize> = BTreeMap::new();
        let mut claimed: BTreeSet<u64> = BTreeSet::new();
        // 1) identity values rendered as text in the string table
        hits.extend(literal_hits(ctx, &enumerators, &mut counts, &mut claimed).await?);
        // 2) name literals, which have no shape and are only candidates because an
        //    enumerating function references them
        hits.extend(name_candidate_hits(ctx, &enumerators).await?);
        // 3) identity values stored as raw bytes in the buffers those functions reach
        hits.extend(binary_hits(ctx, &enumerators).await?);
        // 4) ...and the text renderings the string finder could not see, because the
        //    literal they sit in was never terminated
        hits.extend(blob_literal_hits(ctx, &enumerators, &mut counts, &claimed).await?);
        Ok(hits)
    }
}

// ===========================================================================
// Locating the enumerators
// ===========================================================================

/// Find every function that reads machine identity, by import and by symbol.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[tracing::instrument(name = "host_binding::find_enumerators", skip_all, err(Debug))]
async fn find_enumerators(ctx: &AnalysisContext) -> Result<HashMap<u64, EnumeratingFunction>> {
    let mut found: HashMap<u64, EnumeratingFunction> = HashMap::new();
    // --- imported APIs: resolve each enumerator's call sites to their callers ---
    for import in &ctx.imports {
        let norm = normalize(&import.name);
        let Some(spec) = ENUMERATORS.iter().find(|spec| spec.key == norm) else {
            continue;
        };
        // hop import thunks so the enumeration is attributed to the real caller
        let sites = ctx
            .resolve_call_sites(&import.address)
            .await
            .with_context(|| format!("resolving call sites of import '{}'", import.name))?;
        for site in sites {
            // a call site outside any function tells us nothing about who reads it
            let Some(entry) = site.function else {
                continue;
            };
            let record = found.entry(entry).or_insert_with(|| EnumeratingFunction {
                entry,
                classes: BTreeSet::new(),
                labels: BTreeSet::new(),
                sites: Vec::new(),
            });
            record.classes.insert(spec.class);
            record.labels.insert(spec.label);
            record.sites.push(site.site);
        }
    }
    // --- linked runtimes: the enumerating function *is* the marked symbol ---
    for function in &ctx.functions {
        let Some(marker) = match_runtime_marker(&function.name, RUNTIME_ENUMERATORS, |m| m.marker)
        else {
            continue;
        };
        let Some(entry) =
            parse_addr(&function.entry_point).or_else(|| parse_addr(&function.address))
        else {
            continue;
        };
        let record = found.entry(entry).or_insert_with(|| EnumeratingFunction {
            entry,
            classes: BTreeSet::new(),
            labels: BTreeSet::new(),
            sites: Vec::new(),
        });
        record.classes.insert(marker.class);
        record.labels.insert(marker.label);
    }
    // keep the site lists tidy so the validator sees each site once
    for record in found.values_mut() {
        record.sites.sort_unstable();
        record.sites.dedup();
    }
    Ok(found)
}

/// Report the identity reads themselves as Discovery findings.
///
/// Enumerating machine identity is ordinary on its own — licensing, telemetry,
/// clustering, and crash reporting all do it — so this is [`low`] and carries no
/// artifact. Its value is that it names the functions the analyst should read
/// next, and it is the anchor everything else in this module correlates against.
///
/// # Arguments
///
/// * `ctx` - The analysis context (for function names)
/// * `enumerators` - The functions found reading machine identity
fn enumeration_hits(
    ctx: &AnalysisContext,
    enumerators: &HashMap<u64, EnumeratingFunction>,
) -> Vec<DetectionHit> {
    // stable ordering so the report does not churn between runs
    let mut ordered: Vec<&EnumeratingFunction> = enumerators.values().collect();
    ordered.sort_by_key(|record| record.entry);
    ordered
        .into_iter()
        .map(|record| {
            // anchor at the first call site when there is one, else the function
            let address = record.sites.first().copied().unwrap_or(record.entry);
            let named = ctx
                .function_name(record.entry)
                .map_or_else(|| fmt_addr(record.entry), str::to_string);
            DetectionHit::new(
                T1082,
                address,
                Some(record.entry),
                low(),
                format!("Reads machine identity in {named}: {}", record.describe()),
                format!(
                    "`{named}` asks the operating system for machine-identifying values via {}. \
                     Reading these is ordinary by itself — licensing, telemetry, clustering, and \
                     crash reporting all do it — but it is the read that makes any stored value \
                     of the same class in this program a candidate environmental key.",
                    record.describe(),
                ),
            )
            .kind(EvidenceKind::ContextOnly)
        })
        .collect()
}

/// The runtime enumerator a defined string names, if the string is a symbol name.
///
/// Whole-string equality, deliberately. These runtimes emit their name table
/// NUL-separated so each symbol is its own defined string, while the *other* blob they
/// carry — the literal pool — is unseparated, and a substring search over that would
/// match fragments of unrelated text. Requiring the whole string keeps the two apart
/// without needing to know which blob a given string came from.
///
/// # Arguments
///
/// * `value` - The defined string's value
fn runtime_symbol_for(value: &str) -> Option<&'static RuntimeEnumerator> {
    let name = value.trim();
    RUNTIME_ENUMERATORS
        .iter()
        .find(|entry| name.eq_ignore_ascii_case(entry.marker))
}

/// Report identity enumeration that is only visible as a retained runtime symbol.
///
/// [`enumeration_hits`] matches [`RUNTIME_ENUMERATORS`] against recovered *function
/// names*, which a stripped image does not have — and malware is stripped as a matter
/// of course, so on the binaries this matters most for that path can never fire. What
/// survives stripping is the name table the runtime keeps for its own stack traces:
/// Ghidra reads it as ordinary defined strings, so `net.Interfaces` is still sitting in
/// the image even though no function is called that.
///
/// The string is only evidence that the routine was *linked in*. It says nothing about
/// which function calls it, or whether the program's own code does at all rather than
/// some library it pulled in, so this is [`low`] and [`EvidenceKind::ContextOnly`] and
/// is emitted without a function. It is reported anyway because the alternative on a
/// stripped statically linked sample is to say nothing at all about a capability the
/// image demonstrably carries.
///
/// Matching is whole-string equality, not a substring search. These runtimes emit the
/// name table NUL-separated, so each name is its own defined string, and a substring
/// search would instead run through the *other* blob these binaries carry — the
/// unseparated literal pool — and match on fragments of unrelated text.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `enumerators` - The functions already found by name or import, which suppress this
fn runtime_symbol_hits(
    ctx: &AnalysisContext,
    enumerators: &HashMap<u64, EnumeratingFunction>,
) -> Vec<DetectionHit> {
    // the classes already reported against a real function, which say strictly more
    let attributed: BTreeSet<IdentityClass> = enumerators
        .values()
        .flat_map(|record| record.classes.iter().copied())
        .collect();
    // the best (marker, address) seen for each class, keyed for a stable report
    let mut found: BTreeMap<IdentityClass, (&'static str, u64)> = BTreeMap::new();
    for string in &ctx.strings {
        let Some(marker) = runtime_symbol_for(&string.value) else {
            continue;
        };
        if attributed.contains(&marker.class) {
            continue;
        }
        let Some(address) = parse_addr(&string.address) else {
            continue;
        };
        // first one wins; the rest are the same fact said again
        found.entry(marker.class).or_insert((marker.label, address));
    }
    found
        .into_iter()
        .map(|(class, (label, address))| {
            DetectionHit::new(
                T1082,
                address,
                None,
                low(),
                format!("Linked routine reads the {} ({label})", class.label()),
                format!(
                    "The image retains the runtime symbol `{label}`, which reads the {}. The \
                     symbol table has been stripped, so no function carries that name and the \
                     call site cannot be located statically — this is the routine being present \
                     and reachable, not an observed call, and a library the program links may be \
                     the only thing that reaches it. It does establish that the value is read at \
                     runtime rather than stored in the image, which is why no literal of this \
                     class appears in the string table.",
                    class.label(),
                ),
            )
            .kind(EvidenceKind::ContextOnly)
        })
        .collect()
}

// ===========================================================================
// 1) Shaped literals in the string table
// ===========================================================================

/// Whether another finding of `class` fits under the per-class cap.
///
/// Counts the finding when it does, so a caller only has to ask. The first value a
/// class drops is logged, once, because a silent truncation reads as "that was all
/// of them".
///
/// # Arguments
///
/// * `counts` - The per-class tally, updated in place
/// * `class` - The class the candidate belongs to
/// * `source` - What the tally covers, named in the warning
fn admits(counts: &mut BTreeMap<IdentityClass, usize>, class: IdentityClass, source: &str) -> bool {
    let count = counts.entry(class).or_default();
    // under the cap: take the finding and charge it to the class
    if *count < MAX_PER_CLASS {
        *count += 1;
        return true;
    }
    // exactly at the cap: this is the first value being dropped, so say so and step
    // one past the cap to keep the rest of the class quiet
    if *count == MAX_PER_CLASS {
        tracing::warn!(
            class = class.label(),
            cap = MAX_PER_CLASS,
            source,
            "host-identity cap reached; further values of this class are omitted from the report"
        );
        *count += 1;
    }
    false
}

/// Recover MAC, GUID, and volume-serial literals and attribute each to a function.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `enumerators` - The functions found reading machine identity
/// * `counts` - The per-class cap tally, shared with [`blob_literal_hits`]
/// * `claimed` - The addresses reported here, recorded for [`blob_literal_hits`]
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[tracing::instrument(name = "host_binding::literal_hits", skip_all, err(Debug))]
async fn literal_hits(
    ctx: &AnalysisContext,
    enumerators: &HashMap<u64, EnumeratingFunction>,
    counts: &mut BTreeMap<IdentityClass, usize>,
    claimed: &mut BTreeSet<u64>,
) -> Result<Vec<DetectionHit>> {
    let mut hits = Vec::new();
    // built lazily: only an unreferenced literal needs the data-reference index
    let mut index: Option<Vec<(u64, u64)>> = None;
    for string in &ctx.strings {
        // match the three shapes, already word-bounded and placeholder-filtered
        let matches = find_host_identifiers(&string.value);
        if matches.is_empty() {
            continue;
        }
        let Some(base) = parse_addr(&string.address) else {
            continue;
        };
        for found in matches {
            let class = IdentityClass::from_shape(found.kind);
            // record where the value sits before any cap can drop it, so the raw scan
            // stays quiet about it either way — a capped class is already saturated
            claimed.insert(base.saturating_add(found.at as u64));
            // honor the per-class cap, logging the first value dropped
            if !admits(counts, class, "string table") {
                continue;
            }
            // find the functions that use this literal, preferring real xrefs
            let users =
                literal_users(ctx, &string.address, base, DATA_REF_WINDOW, &mut index).await?;
            hits.push(
                build_identity_hit(
                    ctx,
                    enumerators,
                    class,
                    &found.value,
                    base,
                    &users,
                    &format!("string literal at {}", fmt_addr(base)),
                )
                .await?,
            );
        }
    }
    Ok(hits)
}

/// Resolve the functions that reference a literal, falling back to its buffer.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `address` - The literal's address as the bridge renders it, for the xref query
/// * `base` - The same address, parsed, for the fallback lookup
/// * `window` - How far below `base` the fallback may look for a referenced buffer
/// * `index` - The lazily-built data-reference index
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
async fn literal_users(
    ctx: &AnalysisContext,
    address: &str,
    base: u64,
    window: u64,
    index: &mut Option<Vec<(u64, u64)>>,
) -> Result<Vec<u64>> {
    let mut users: Vec<u64> = Vec::new();
    // code cross-references are the strongest attribution available
    let xrefs = ctx
        .xrefs_to(address)
        .await
        .with_context(|| format!("resolving references to literal at {address}"))?;
    for xref in xrefs {
        if let Some(site) = parse_addr(&xref.from) {
            if let Some(entry) = ctx.containing_function(site) {
                users.push(entry);
            }
        }
    }
    // no xref: fall back to whoever references the buffer this literal sits in
    if users.is_empty() {
        if index.is_none() {
            *index = Some(ctx.reference_index().await?);
        }
        if let Some(entry) = index
            .as_ref()
            .and_then(|index| referencing_function(index, base, window))
        {
            users.push(entry);
        }
    }
    users.sort_unstable();
    users.dedup();
    Ok(users)
}

// ===========================================================================
// 2) Name candidates inside enumerating functions
// ===========================================================================

/// Recover the name literals an enumerating function compares against.
///
/// Computer, domain, and account names have no distinguishing shape, so the only
/// generic way to find them is by where they are used: a literal referenced by
/// the same function that just asked the OS for a name is a candidate expected
/// value. Nothing outside such a function is considered, because outside one the
/// filter would accept most short identifiers in the binary.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `enumerators` - The functions found reading machine identity
///
/// # Errors
///
/// Returns an error if a bridge query fails.
#[tracing::instrument(name = "host_binding::name_candidate_hits", skip_all, err(Debug))]
async fn name_candidate_hits(
    ctx: &AnalysisContext,
    enumerators: &HashMap<u64, EnumeratingFunction>,
) -> Result<Vec<DetectionHit>> {
    // only the functions that read a *name* can make a name literal a candidate
    let readers: Vec<&EnumeratingFunction> = {
        let mut readers: Vec<&EnumeratingFunction> = enumerators
            .values()
            .filter(|record| {
                record.classes.contains(&IdentityClass::MachineName)
                    || record.classes.contains(&IdentityClass::AccountName)
            })
            .collect();
        readers.sort_by_key(|record| record.entry);
        readers
    };
    if readers.is_empty() {
        return Ok(Vec::new());
    }
    // index the string table by address so each xref can be resolved back to a value
    let by_address: HashMap<u64, &ghidra_cli::models::StringData> = ctx
        .strings
        .iter()
        .filter_map(|string| parse_addr(&string.address).map(|addr| (addr, string)))
        .collect();
    let mut hits = Vec::new();
    for record in readers {
        // a runtime/library helper reading its own hostname is not the author's check
        if ctx
            .function_name(record.entry)
            .is_some_and(is_library_function)
        {
            continue;
        }
        // the class to report: prefer the account reading when both are present,
        // since an account name is the narrower claim
        let class = if record.classes.contains(&IdentityClass::AccountName) {
            IdentityClass::AccountName
        } else {
            IdentityClass::MachineName
        };
        // every static data address this function reaches that resolves to a string
        let candidates = referenced_names(ctx, record.entry, &by_address).await?;
        for value in candidates.into_iter().take(MAX_NAMES_PER_FUNCTION) {
            hits.push(
                build_identity_hit(
                    ctx,
                    enumerators,
                    class,
                    &value.text,
                    value.address,
                    &[record.entry],
                    &format!(
                        "literal at {} referenced by the enumerating function",
                        fmt_addr(value.address)
                    ),
                )
                .await?,
            );
        }
    }
    Ok(hits)
}

/// One name literal a function references.
#[derive(Debug, Clone)]
struct NameCandidate {
    /// The literal's address.
    address: u64,
    /// The literal itself.
    text: String,
}

/// The plausible name literals a function references.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `entry` - The function to read
/// * `by_address` - The string table indexed by address
///
/// # Errors
///
/// Returns an error if disassembling the function fails.
async fn referenced_names(
    ctx: &AnalysisContext,
    entry: u64,
    by_address: &HashMap<u64, &ghidra_cli::models::StringData>,
) -> Result<Vec<NameCandidate>> {
    // read the function's static data references
    let insns = ctx
        .disasm_fn(entry)
        .await
        .with_context(|| format!("disassembling enumerating function at {}", fmt_addr(entry)))?;
    let mut candidates = Vec::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    for addr in crate::thorpal::detect::decode::buffers::data_addrs(&insns) {
        // one candidate per address, in first-reference order
        if !seen.insert(addr) {
            continue;
        }
        let Some(string) = by_address.get(&addr) else {
            continue;
        };
        // the shape filter: identifiers in, prose/paths/templates/filenames out
        if !plausible_host_name(&string.value) {
            continue;
        }
        candidates.push(NameCandidate {
            address: addr,
            text: string.value.trim().to_string(),
        });
    }
    Ok(candidates)
}

// ===========================================================================
// 3) Binary identifiers in the buffers enumerators reach
// ===========================================================================

/// Recover six-byte MAC and sixteen-byte GUID values stored as raw bytes.
///
/// Scoped hard: only the buffers an enumerating function references are read,
/// and only when the program enumerates that class of identity at all. An
/// unscoped six-byte scan matches constantly, and an unscoped sixteen-byte GUID
/// scan matches every compiled-in interface identifier a COM client carries.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `enumerators` - The functions found reading machine identity
///
/// # Errors
///
/// Returns an error if reading the program's sections or references fails.
#[tracing::instrument(name = "host_binding::binary_hits", skip_all, err(Debug))]
async fn binary_hits(
    ctx: &AnalysisContext,
    enumerators: &HashMap<u64, EnumeratingFunction>,
) -> Result<Vec<DetectionHit>> {
    // nothing enumerates identity, so nothing scopes the scan
    if enumerators.is_empty() {
        return Ok(Vec::new());
    }
    // which classes the program actually reads — a MAC scan is only justified by
    // adapter enumeration, and a GUID scan only by a machine-GUID read
    let read: BTreeSet<IdentityClass> = enumerators
        .values()
        .flat_map(|record| record.classes.iter().copied())
        .collect();
    if !read.contains(&IdentityClass::MacAddress) && !read.contains(&IdentityClass::Guid) {
        return Ok(Vec::new());
    }
    let sections = ctx.sections().await?;
    let index = ctx.reference_index().await?;
    let mut hits = Vec::new();
    let mut counts: BTreeMap<IdentityClass, usize> = BTreeMap::new();
    for ((class, at, value), (section, users)) in
        collect_stored_values(&sections, &index, enumerators)
    {
        // honor the per-class cap, logging the first value dropped
        if !admits(&mut counts, class, "raw bytes") {
            continue;
        }
        hits.push(
            build_identity_hit(
                ctx,
                enumerators,
                class,
                &value,
                at,
                &users,
                &format!(
                    "raw bytes at {} in `{}`, inside a buffer the enumerating function \
                     references",
                    fmt_addr(at),
                    section.name
                ),
            )
            .await?,
        );
    }
    Ok(hits)
}

/// Find the stored identity values the enumerating functions reach.
///
/// Returns one entry per value, keyed by what it is and where it is, holding the
/// block it was found in and every enumerating function that reaches it.
///
/// Collecting before building is what makes a stored value one finding. The same
/// bytes are found again from every enumerating function whose referenced buffer
/// covers them — several routines reading identity commonly share one scratch
/// structure — and each rediscovery is not another value, it is another user of the
/// one value. Reported separately they would fill a report with copies of a single
/// address and exhaust the class's whole allowance on it; collected, the extra
/// functions become what they are, which is more attribution for one finding.
///
/// The scan each function drives is scoped to what *that* function reads rather
/// than to what the program reads somewhere. The only thing that makes six bytes a
/// hardware address rather than six bytes is that they sit in a buffer belonging to
/// a routine that handles hardware addresses, so a routine that reads the account
/// name lends the pattern nothing, and letting it do so both invents values and
/// credits them to a function that never touched one.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `index` - Data addresses paired with the function that references them
/// * `enumerators` - The functions observed reading host identity, by entry address
fn collect_stored_values<'a>(
    sections: &'a [Section],
    index: &[(u64, u64)],
    enumerators: &HashMap<u64, EnumeratingFunction>,
) -> BTreeMap<(IdentityClass, u64, String), (&'a Section, Vec<u64>)> {
    let mut candidates: BTreeMap<(IdentityClass, u64, String), (&Section, Vec<u64>)> =
        BTreeMap::new();
    // walk only the data addresses the enumerating functions reference
    for (addr, entry) in index {
        let Some(record) = enumerators.get(entry) else {
            continue;
        };
        // scan for the classes this function was seen reading, and only the two the
        // byte scan can look for
        let read: BTreeSet<IdentityClass> = record
            .classes
            .iter()
            .copied()
            .filter(|class| matches!(class, IdentityClass::MacAddress | IdentityClass::Guid))
            .collect();
        if read.is_empty() {
            continue;
        }
        // read a short window; identity structures sit next to the pointer
        let Some((section, window)) = window_at(sections, *addr, BINARY_SCAN_WINDOW) else {
            continue;
        };
        // a resource/asset block holds icons and manifests, not runtime identity
        if is_asset_section(&section.name) {
            continue;
        }
        // and a code block holds instructions, not a stored field — see
        // `holds_stored_data`
        if !holds_stored_data(section) {
            continue;
        }
        for (offset, class, value) in scan_window(window, &read) {
            let at = addr.saturating_add(offset as u64);
            // a candidate sharing bytes with a stored pointer is a slice of a pointer
            // table, not a value — this is judged against the whole block rather than
            // the window so a candidate at the window's edge is still judged
            if straddles_pointer(sections, section, at, class.byte_len()) {
                continue;
            }
            let users = &mut candidates
                .entry((class, at, value))
                .or_insert_with(|| (section, Vec::new()))
                .1;
            // one function can reference two buffers that both cover the value
            if !users.contains(entry) {
                users.push(*entry);
            }
        }
    }
    candidates
}

/// Read up to `len` bytes of the section containing `addr`.
///
/// Returns the section itself alongside the bytes so the caller can judge where the
/// window came from — its name and its permissions both decide whether a byte
/// pattern found inside it means anything — without re-searching for it.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `addr` - The address to read from
/// * `len` - The most bytes to return
fn window_at(sections: &[Section], addr: u64, len: usize) -> Option<(&Section, &[u8])> {
    // locate the block holding the address, skipping ones with no bytes read and
    // ones that are not part of the process image at all
    let section = sections
        .iter()
        .find(|section| section.contains(addr) && section.is_loaded && !section.bytes.is_empty())?;
    // convert to an offset within the block and clamp the window to its end
    let offset = usize::try_from(addr - section.base).ok()?;
    let end = offset.saturating_add(len).min(section.bytes.len());
    (offset < end).then(|| (section, &section.bytes[offset..end]))
}

/// Whether a section holds packaged assets rather than runtime data.
///
/// # Arguments
///
/// * `name` - The section name
fn is_asset_section(name: &str) -> bool {
    // resource and debug blocks carry icons, manifests, and type info; a machine
    // identity found in one is part of packaged content, not a runtime check
    let lower = name.to_ascii_lowercase();
    lower.contains("rsrc") || lower.contains("resource") || lower.contains("debug")
}

/// Whether a section holds stored fields rather than instructions.
///
/// A six-byte scan needs a haystack where six consecutive bytes plausibly form one
/// value. Data blocks are such a haystack: a stored address is written as a field and
/// read back as a field, so a match is aligned with something the program treats as a
/// unit. Instruction bytes are not. A code block is a dense stream in which the byte
/// boundaries belong to opcodes and operands, and a window slid across it produces
/// address-shaped sextets at whatever offsets the encoding happens to allow — the
/// giveaway is that several of them overlap, being different views of the same bytes,
/// which no stored field can be.
///
/// Executability is the test rather than a name list because it is the property that
/// actually matters and it holds across formats; a scan restricted to blocks Ghidra
/// says are not executed cannot read code as data whatever the block is called. Note
/// this also excludes the writable-and-executable block a runtime-obfuscated sample
/// carries, which is correct for a different reason: before it is written to it holds
/// a compressed or encrypted stream, and a six-byte pattern found in high-entropy
/// noise is chance.
///
/// # Arguments
///
/// * `section` - The section the window was read from
fn holds_stored_data(section: &Section) -> bool {
    !section.is_executable()
}

/// Whether a candidate overlaps a stored pointer, and so is a slice of one.
///
/// The neighbourhood tests in [`scan_window`] all ask what a region is made of —
/// text, terminators, small integers on a stride. This one asks a different and
/// sharper question, and gets a definite answer: a byte that belongs to a stored
/// address cannot also belong to a hardware address, so a candidate overlapping one
/// is not a value, it is a window that happens to span the end of one field and the
/// start of the next.
///
/// The reason it matters is that pointer tables are where the remaining fabrications
/// come from once the text tests are in place, and every compiled binary is full of
/// them: vtables, relocated data pointers, jump tables, and — the densest source,
/// because each of its entries is a pointer followed by a mangled type name, so the
/// boundary between plain text and a pointer recurs every few bytes — a C++ runtime's
/// type-descriptor table.
///
/// A word counts only when it is aligned to its own width and decodes to an address
/// inside a loaded block, which is what keeps this from rejecting real values: for
/// six arbitrary bytes to be read as part of a pointer, the bytes beside them have to
/// land in the program's own address range, and the ranges a loaded image occupies
/// are a vanishing fraction of what sixty-four bits can express.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map, used to test where a word points
/// * `section` - The block the candidate was found in
/// * `at` - The candidate's address
/// * `len` - The candidate's length in bytes
fn straddles_pointer(sections: &[Section], section: &Section, at: u64, len: u64) -> bool {
    POINTER_WIDTHS.iter().any(|width| {
        // the aligned words of this width that share a byte with the candidate
        let mut word = at - at % width;
        while word < at + len {
            if let Some(target) = read_word(section, word, *width) {
                // a word pointing into the image is a pointer, and its bytes are spoken for
                if sections
                    .iter()
                    .any(|section| section.is_loaded && section.contains(target))
                {
                    return true;
                }
            }
            word += width;
        }
        false
    })
}

/// Read a little-endian word of `width` bytes from a section.
///
/// Returns `None` when the word runs past the bytes read for the block, or when it is
/// zero — a zeroed slot is uninitialized or a null terminator, not a stored address,
/// and treating it as one would reject every candidate next to padding.
///
/// # Arguments
///
/// * `section` - The block to read from
/// * `at` - The address to read at
/// * `width` - The word width in bytes
fn read_word(section: &Section, at: u64, width: u64) -> Option<u64> {
    // convert to an offset within the block, rejecting an address outside it
    let start = usize::try_from(at.checked_sub(section.base)?).ok()?;
    let end = start.checked_add(usize::try_from(width).ok()?)?;
    let bytes = section.bytes.get(start..end)?;
    // little-endian, widened to the largest width so both cases share one path
    let value = bytes
        .iter()
        .rev()
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
    (value != 0).then_some(value)
}

/// Find binary MAC and GUID values inside a window of bytes.
///
/// Returns `(offset, class, rendered value)` per match.
///
/// # Arguments
///
/// * `window` - The bytes to scan
/// * `read` - The identity classes the program was observed enumerating
fn scan_window(
    window: &[u8],
    read: &BTreeSet<IdentityClass>,
) -> Vec<(usize, IdentityClass, String)> {
    let mut found = Vec::new();
    // GUIDs first: sixteen bytes with real structure, so they are far safer to
    // claim than six arbitrary ones, and they consume the bytes a MAC would take.
    // The search runs whether or not the program was seen reading a GUID, because
    // its two jobs are separable: only an observed enumerator makes a GUID worth
    // *reporting*, but the bytes are spoken for either way, and a well-known device
    // or interface class GUID sliced six bytes at a time is otherwise one of the
    // richest sources of fabricated addresses a Windows binary has
    let mut claimed: Vec<(usize, usize)> = Vec::new();
    let report_guids = read.contains(&IdentityClass::Guid);
    // step over a match rather than past it by one: two GUIDs cannot overlap, so
    // a byte already inside one is not the start of another
    let mut offset = 0;
    while offset + 16 <= window.len() {
        let bytes = &window[offset..offset + 16];
        if let Some(value) = render_guid(bytes) {
            if report_guids {
                found.push((offset, IdentityClass::Guid, value));
            }
            claimed.push((offset, offset + 16));
            offset += 16;
            continue;
        }
        offset += 1;
    }
    if read.contains(&IdentityClass::MacAddress) {
        // same non-overlap rule, and it matters far more here: six bytes carry no
        // internal structure, so a table this slides across yields a fresh "address"
        // at every single byte offset, each one a shifted copy of the last
        let mut offset = 0;
        while offset + 6 <= window.len() {
            // never re-report bytes a GUID already accounts for
            if let Some((_, end)) = claimed
                .iter()
                .find(|(start, end)| offset < *end && *start < offset + 6)
            {
                offset = *end;
                continue;
            }
            // six bytes cannot tell a table apart from an address, nor a fragment of
            // a string apart from one, so ask the bytes around them: a candidate
            // lying inside a stride table with a column that is empty in every
            // element is a slice of that table, most often one straddling the
            // boundary between two of its elements, a candidate lying inside a run
            // of text is a slice of the text, and a candidate whose neighbourhood is
            // nothing but text and terminators is a slice of a table of short strings
            // that no single run of is long enough to explain it
            if !in_int_table(window, offset)
                && !in_text_run(window, offset)
                && !in_string_table(window, offset)
            {
                if let Some(value) = render_mac(&window[offset..offset + 6]) {
                    found.push((offset, IdentityClass::MacAddress, value));
                    offset += 6;
                    continue;
                }
            }
            offset += 1;
        }
    }
    found.sort_by_key(|(offset, _, _)| *offset);
    found
}

/// Render sixteen bytes as a GUID, if they are structurally a GUID.
///
/// Requires the RFC 4122 version and variant fields, which is what makes a
/// sixteen-byte scan defensible: the two fields together rule out roughly
/// sixty-three of every sixty-four arbitrary byte runs. The layout is the
/// little-endian one Windows uses, since that is the form a program that read a
/// machine GUID from the OS would have in memory.
///
/// # Arguments
///
/// * `bytes` - Exactly sixteen bytes
fn render_guid(bytes: &[u8]) -> Option<String> {
    // the all-zero GUID is the "unset" sentinel every API returns on failure
    if bytes.iter().all(|b| *b == 0) || bytes.iter().all(|b| *b == 0xff) {
        return None;
    }
    // the version nibble: 1..=5 are the defined UUID versions
    let version = bytes[7] >> 4;
    if !(1..=5).contains(&version) {
        return None;
    }
    // the variant bits: `10xxxxxx` is the RFC 4122 variant
    if bytes[8] & 0xc0 != 0x80 {
        return None;
    }
    // the first three fields are little-endian, the last two big-endian
    Some(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}",
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_le_bytes([bytes[4], bytes[5]]),
        u16::from_le_bytes([bytes[6], bytes[7]]),
        bytes[8],
        bytes[9],
        bytes[10..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    ))
}

/// Whether the six bytes at `offset` sit inside a table of little-endian integers.
///
/// [`render_mac`] judges a candidate on its own six bytes, which is all it has, and
/// that is not enough against the shape that produces most fabricated addresses: a
/// compiler-emitted table of offsets or pointers, whose elements are far smaller
/// than the width they are stored in and so carry a column of zeros at every stride.
/// A six-byte window slid across one of those picks up the tail of one element and
/// the head of the next, and the result is two zero bytes and four plausible ones —
/// one short of the all-zero-OUI and half-zeros bars, and indistinguishable from a
/// real address without looking wider.
///
/// So look wider. [`crate::thorpal::detect::decode::recover::is_wide_int_table`] already names this
/// exact layout for the decode engine; the same test over the candidate's
/// neighbourhood answers the same question here.
///
/// # Arguments
///
/// * `window` - The scanned bytes
/// * `offset` - Where in `window` the six-byte candidate starts
fn in_int_table(window: &[u8], offset: usize) -> bool {
    // read the candidate plus a margin either side, clamped to the window
    let start = offset.saturating_sub(MAC_CONTEXT_RADIUS);
    let end = offset
        .saturating_add(6 + MAC_CONTEXT_RADIUS)
        .min(window.len());
    crate::thorpal::detect::decode::recover::is_wide_int_table(&window[start..end])
}

/// Whether the six bytes at `offset` sit inside a run of text.
///
/// The other half of what [`render_mac`] cannot see from six bytes. A binary's data
/// blocks are mostly strings — program names, format templates, usage banners, log
/// messages — and a six-byte window slid along one of those produces an address at
/// every offset, each a shifted copy of the last, all of them plausible: text bytes
/// are non-zero, non-repeating, and rarely all-even, so every filter in `render_mac`
/// passes. String terminators make it worse rather than better, because the pair of
/// NULs between two literals reads as the leading zeros of a sparse-looking address.
///
/// So measure the run the candidate belongs to. The candidate's bytes are extended
/// through their printable neighbours in both directions; if the run that results is
/// long enough to be a real string ([`MAC_TEXT_RUN`]) and enough of the candidate is
/// inside it ([`MAC_TEXT_OVERLAP`]), the string explains the bytes and no address is
/// claimed. An adapter address stored next to a string is untouched: it overlaps that
/// string's run by at most a byte or two of its own.
///
/// # Arguments
///
/// * `window` - The scanned bytes
/// * `offset` - Where in `window` the six-byte candidate starts
fn in_text_run(window: &[u8], offset: usize) -> bool {
    // the printable bytes of the candidate itself, as a span within it — a candidate
    // split by a NUL is judged on the longer side, since that is the side that
    // continues into a string
    let Some((seed_start, seed_len)) = longest_text_span(&window[offset..offset + 6]) else {
        return false;
    };
    if seed_len < MAC_TEXT_OVERLAP {
        return false;
    }
    // walk the run out through the neighbouring bytes in both directions
    let mut start = offset + seed_start;
    let mut end = start + seed_len;
    while start > 0 && is_text_byte(window[start - 1]) {
        start -= 1;
    }
    while end < window.len() && is_text_byte(window[end]) {
        end += 1;
    }
    end - start >= MAC_TEXT_RUN
}

/// Whether the six bytes at `offset` sit inside a table of short strings.
///
/// The gap between the other two neighbourhood tests. [`in_int_table`] wants a column
/// of zeros on a stride and [`in_text_run`] wants one run of [`MAC_TEXT_RUN`] bytes;
/// a run of option names or error strings is neither — each string is too short to
/// clear the run bar on its own, and the NULs between them fall wherever the strings
/// end rather than on any stride. What the region does have is composition: it is
/// made of printable bytes and terminators and nothing else, which data holding a
/// stored address is not ([`MAC_STRING_TABLE_SHARE`] records the measurement).
///
/// # Arguments
///
/// * `window` - The scanned bytes
/// * `offset` - Where in `window` the six-byte candidate starts
fn in_string_table(window: &[u8], offset: usize) -> bool {
    // read the candidate plus a margin either side, clamped to the window
    let start = offset.saturating_sub(MAC_CONTEXT_RADIUS);
    let end = offset
        .saturating_add(6 + MAC_CONTEXT_RADIUS)
        .min(window.len());
    let context = &window[start..end];
    // a neighbourhood too small to have a composition is not evidence of one
    if context.len() < 6 + MAC_CONTEXT_RADIUS {
        return false;
    }
    // every byte either belongs to a string or terminates one
    let textual = context
        .iter()
        .filter(|byte| is_text_byte(**byte) || **byte == 0)
        .count();
    textual * 100 >= context.len() * MAC_STRING_TABLE_SHARE
}

/// Whether a byte is one a string would be made of.
///
/// Printable ASCII plus the three whitespace controls a banner or a log template
/// carries. Deliberately not the high range: a byte over `0x7f` is as likely to be
/// binary as it is to be one half of a UTF-8 codepoint, and reading it as text would
/// let any dense region pass for a string.
///
/// # Arguments
///
/// * `byte` - The byte to classify
fn is_text_byte(byte: u8) -> bool {
    (0x20..=0x7e).contains(&byte) || matches!(byte, b'\t' | b'\n' | b'\r')
}

/// The longest run of text bytes in `bytes`, as `(start, length)`.
///
/// # Arguments
///
/// * `bytes` - The bytes to search
fn longest_text_span(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut run_start = None;
    // close the open run at every non-text byte, and once more past the end
    for index in 0..=bytes.len() {
        match (index < bytes.len() && is_text_byte(bytes[index]), run_start) {
            (true, None) => run_start = Some(index),
            (false, Some(start)) => {
                let len = index - start;
                if best.is_none_or(|(_, best_len)| len > best_len) {
                    best = Some((start, len));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    best
}

/// Render six bytes as a MAC address, if they could plausibly be one.
///
/// Six bytes have no internal structure to check, so the only available filters are
/// the degenerate cases [`is_degenerate_mac`] names. What actually makes this
/// defensible is the caller's scoping — the bytes come from a buffer an
/// adapter-enumerating function references — not this filter.
///
/// # Arguments
///
/// * `bytes` - Exactly six bytes
fn render_mac(bytes: &[u8]) -> Option<String> {
    // the sentinels, fill patterns, counters, group addresses, and sparse windows
    // that are the only thing six bytes on their own can be judged against
    if is_degenerate_mac(bytes) {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

// ===========================================================================
// 4) Shaped literals in a concatenated literal pool
// ===========================================================================

/// Recover identity literals from the raw bytes of the initialized data blocks.
///
/// [`literal_hits`] can only see what the string finder defined as a string, and
/// there is a whole class of binary where that is almost nothing. Some runtimes
/// emit every literal in the program into one contiguous pool with no terminators
/// between them, carrying each length at the use site instead. The string finder
/// needs a terminator to know where a string ends, so on such an image it either
/// skips the pool or swallows a large stretch of it as one enormous string —
/// and an identity value sitting inside is invisible to the string-table pass
/// either way. Reading the block's bytes directly is the only way to reach it.
///
/// What that costs is the terminator that used to bound a match, and what replaces
/// it is described on [`crate::thorpal::detect::indicators::find_blob_host_identifiers`], which is
/// also where the corpus measurement behind the looser rule is recorded. The
/// volume-serial shape is deliberately not searched by that function: unterminated,
/// it also describes the tail of a version string, which is the one thing a literal
/// pool is full of.
///
/// # Why this is not scoped to the enumerating functions
///
/// [`binary_hits`] searches only the buffers an enumerating function references,
/// because six raw bytes on their own mean nothing. A rendered address is a
/// different proposition — seventeen characters in a fixed grammar, with the
/// degenerate renderings already filtered — so the shape carries its own weight and
/// a hit with no enumerator behind it is simply reported at [`low`], exactly as the
/// string-table pass reports one.
///
/// Scoping it that way would also blind it on precisely the images it exists for. A
/// statically linked runtime is recognized here through a symbol marker rather than
/// an import, and a marker yields no call site, so those samples have no enumerating
/// function to scope a scan to at all.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `enumerators` - The functions found reading machine identity
/// * `counts` - The per-class cap tally, shared with [`literal_hits`]
/// * `claimed` - The addresses [`literal_hits`] already reported
///
/// # Errors
///
/// Returns an error if a bridge query fails.
#[tracing::instrument(name = "host_binding::blob_literal_hits", skip_all, err(Debug))]
async fn blob_literal_hits(
    ctx: &AnalysisContext,
    enumerators: &HashMap<u64, EnumeratingFunction>,
    counts: &mut BTreeMap<IdentityClass, usize>,
    claimed: &BTreeSet<u64>,
) -> Result<Vec<DetectionHit>> {
    let sections = ctx.sections().await?;
    // built lazily, and only reached by a literal with no reference of its own
    let mut index: Option<Vec<(u64, u64)>> = None;
    let mut hits = Vec::new();
    for section in &sections {
        // a literal pool is initialized, loaded, and not executed; a resource block
        // holds packaged content rather than the program's own strings
        if !section.is_loaded
            || section.bytes.is_empty()
            || section.is_executable()
            || is_asset_section(&section.name)
        {
            continue;
        }
        // a block cut short by the bridge's read limit is scanned as far as it was
        // read, which is worth saying out loud since the tail is simply not searched
        if section.truncated {
            tracing::debug!(
                section = section.name,
                read = section.bytes.len(),
                "scanning a truncated block for pooled identity literals"
            );
        }
        for found in find_blob_host_identifiers(&section.bytes) {
            let class = IdentityClass::from_shape(found.kind);
            let at = section.base.saturating_add(found.at as u64);
            // the string finder got to this one first, so it is already reported
            if claimed.contains(&at) {
                continue;
            }
            // honor the per-class cap, logging the first value dropped
            if !admits(counts, class, "literal pool") {
                continue;
            }
            // attribute it the same way a defined string is attributed, but with a
            // tighter fallback window — see `BLOB_REF_WINDOW`
            let users = literal_users(ctx, &fmt_addr(at), at, BLOB_REF_WINDOW, &mut index).await?;
            hits.push(
                build_identity_hit(
                    ctx,
                    enumerators,
                    class,
                    &found.value,
                    at,
                    &users,
                    &format!(
                        "literal at {} in `{}`, recovered from the block's bytes rather than \
                         the string table",
                        fmt_addr(at),
                        section.name
                    ),
                )
                .await?,
            );
        }
    }
    Ok(hits)
}

// ===========================================================================
// Building the finding
// ===========================================================================

/// Build one host-identity finding, scored by how well it correlates.
///
/// This is where the module's whole false-positive contract lives: the value's
/// shape sets nothing but the class, and everything about the score comes from
/// whether an enumerating function uses it and whether a comparison was actually
/// observed.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `enumerators` - The functions found reading machine identity
/// * `class` - What the value identifies
/// * `value` - The recovered value itself
/// * `address` - Where the value lives
/// * `users` - The functions that reference it, in address order
/// * `origin` - A phrase describing where the value was recovered from
///
/// # Errors
///
/// Returns an error if decompiling a correlated function fails irrecoverably.
async fn build_identity_hit(
    ctx: &AnalysisContext,
    enumerators: &HashMap<u64, EnumeratingFunction>,
    class: IdentityClass,
    value: &str,
    address: u64,
    users: &[u64],
    origin: &str,
) -> Result<DetectionHit> {
    // the strongest attribution available: a user that also enumerates this very
    // class of identity, then any user that enumerates identity at all
    let matched = users
        .iter()
        .filter_map(|entry| enumerators.get(entry))
        .find(|record| record.classes.contains(&class))
        .or_else(|| {
            users
                .iter()
                .filter_map(|entry| enumerators.get(entry))
                .next()
        });
    // the function the artifact is attributed to
    let primary = matched
        .map(|record| record.entry)
        .or_else(|| users.first().copied());
    let named = primary
        .map(|entry| {
            ctx.function_name(entry)
                .map_or_else(|| fmt_addr(entry), str::to_string)
        })
        .unwrap_or_default();
    // when the value is used by an enumerating function, look for the comparison
    // that would make it an actual guardrail rather than a stored constant
    let comparison = match matched {
        Some(record) => observed_comparison(ctx, record.entry, value).await,
        None => None,
    };
    // scoring: the correlation and the comparison are the whole basis
    let (severity, kind) = match (matched, comparison) {
        // a comparison feeding a branch is the guardrail itself; the validator
        // re-checks the control flow, which is what `ControlFlowGated` requests
        (Some(_), Some(true)) => (medium(), EvidenceKind::ControlFlowGated),
        // used by the enumerating function, but no comparison was visible
        (Some(_), _) => (medium(), EvidenceKind::ContextOnly),
        // a bare literal of the right shape and nothing corroborating it
        (None, _) => (low(), EvidenceKind::ContextOnly),
    };
    // the headline: the value, its class, and who reads it
    let content = match matched {
        Some(record) => format!(
            "Stored {} `{value}` used by {named}, which reads the same class of value via {}",
            class.label(),
            record.describe(),
        ),
        None => format!("Stored {} `{value}`", class.label()),
    };
    // the reasoning: what was recovered, what corroborates it, and what does not
    let mut reasoning = format!(
        "A {} is stored in this program as {origin}. ",
        class.label()
    );
    match (matched, comparison) {
        (Some(record), Some(true)) => reasoning.push_str(&format!(
            "`{named}` obtains the machine's own value via {} and the decompilation shows this \
             literal compared inside a branch condition — the program takes a different path \
             depending on whether it is running on this specific machine. That is an execution \
             guardrail: on any other host the guarded code never runs, so both sandbox detonation \
             and a static read that misses the check will show the sample doing nothing.",
            record.describe(),
        )),
        (Some(record), _) => reasoning.push_str(&format!(
            "`{named}` obtains the machine's own value via {} and also references this literal, so \
             the program holds both the live value and a stored one of the same class in the same \
             place. No comparison was visible in the decompilation, so this is co-location rather \
             than an observed check — the literal may equally be a default, a log field, or a \
             value being sent rather than tested.",
            record.describe(),
        )),
        (None, _) => {
            reasoning.push_str(
                "Nothing in this program was observed reading the machine's own value of this \
                 class, so there is no evidence the literal is used as a guardrail. It is \
                 reported because the value itself is pivotable — it names a specific machine, \
                 adapter, or account — but a literal of this shape is equally often a vendor \
                 identifier, a protocol constant, a test vector, or documentation.",
            );
            if !named.is_empty() {
                reasoning.push_str(&format!(" It is referenced by `{named}`."));
            }
        }
    }
    // the artifact: value plus the attribution the flag boundary would drop
    let mut artifact = HitArtifact::new(class.artifact(), value);
    // the role must name the *observed* relationship, never a guessed one
    if let Some(record) = matched {
        let role = match comparison {
            Some(true) => format!(
                "reads the machine's own {} via {} and compares this value in a branch condition",
                class.label(),
                record.describe(),
            ),
            _ => format!(
                "reads the machine's own {} via {} and references this value",
                class.label(),
                record.describe(),
            ),
        };
        artifact = artifact.role(role);
    } else if primary.is_some() {
        artifact = artifact.role("references this value; no identity enumeration observed here");
    }
    // supplementary context: the other users, or why there is no attribution
    artifact = artifact.detail(attribution_detail(ctx, users, primary, origin));
    Ok(
        DetectionHit::new(T1480_001, address, primary, severity, content, reasoning)
            .kind(kind)
            .artifact(artifact),
    )
}

/// Describe the remaining users, or say plainly that attribution failed.
///
/// # Arguments
///
/// * `ctx` - The analysis context (for function names)
/// * `users` - Every function referencing the value
/// * `primary` - The function the artifact was attributed to
/// * `origin` - Where the value was recovered from
fn attribution_detail(
    ctx: &AnalysisContext,
    users: &[u64],
    primary: Option<u64>,
    origin: &str,
) -> String {
    // no function reaches this value at all — say so rather than leaving a blank
    // field that reads as though attribution succeeded
    let Some(primary) = primary else {
        return format!(
            "Recovered from {origin}. No function was found referencing this value, so it could \
             not be attributed to code — it is reported unattributed."
        );
    };
    let mut detail = format!("Recovered from {origin}.");
    // every other function that also reaches it, so the analyst sees the full set
    let others: Vec<String> = users
        .iter()
        .filter(|entry| **entry != primary)
        .map(|entry| {
            ctx.function_name(*entry)
                .map_or_else(|| fmt_addr(*entry), str::to_string)
        })
        .collect();
    if !others.is_empty() {
        detail.push_str(&format!(" Also referenced by: {}.", others.join(", ")));
    }
    detail
}

/// Whether a function's decompilation compares `value`, and whether in a branch.
///
/// Returns `Some(true)` when the comparison appears in a branch condition,
/// `Some(false)` when a comparison is visible but not obviously guarding
/// anything, and `None` when no comparison was found (which includes the case
/// where the decompiler failed — a failure is not evidence of absence, so the
/// caller treats it the same as "not observed" and scores no higher).
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `entry` - The function to read
/// * `value` - The literal to look for
async fn observed_comparison(ctx: &AnalysisContext, entry: u64, value: &str) -> Option<bool> {
    // a decompiler failure is common and non-fatal; it simply yields no evidence
    let decompiled = match ctx.decompile_fn(entry).await {
        Ok(text) => text,
        Err(error) => {
            tracing::debug!(
                function = %fmt_addr(entry),
                ?error,
                "decompilation unavailable; no comparison evidence for this value"
            );
            return None;
        }
    };
    comparison_in(&decompiled, value)
}

/// Find `value` being compared in decompiled C, and say whether it guards a branch.
///
/// Kept pure (text in, verdict out) so the guards are unit-testable without Ghidra.
///
/// # Arguments
///
/// * `decompiled` - The function's decompiled C
/// * `value` - The literal to look for
fn comparison_in(decompiled: &str, value: &str) -> Option<bool> {
    /// The comparison forms a decompiled equality check appears as.
    const COMPARISONS: &[&str] = &[
        "strcmp",
        "strncmp",
        "stricmp",
        "strcasecmp",
        "strncasecmp",
        "wcscmp",
        "wcsncmp",
        "wcsicmp",
        "memcmp",
        "comparestring",
        "lstrcmp",
        "==",
        "!=",
    ];
    /// The tokens that mark a branch condition in decompiled C.
    const BRANCHES: &[&str] = &["if (", "if(", "while (", "while(", "} else", "? "];
    let mut seen = false;
    for line in decompiled.lines() {
        // the decompiler renders a string literal inline and quoted
        if !line.contains(value) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if !COMPARISONS.iter().any(|token| lower.contains(token)) {
            continue;
        }
        seen = true;
        // a comparison inside a branch condition is the guardrail shape
        if BRANCHES.iter().any(|token| lower.contains(token)) {
            return Some(true);
        }
    }
    seen.then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime symbol is recognized as the whole string, never as a fragment of the
    /// unseparated literal pool a statically linked image also carries.
    #[test]
    fn runtime_symbol_for_matches_whole_names_only() {
        // the name table entry, as the runtime writes it
        let found = runtime_symbol_for("net.Interfaces").expect("a linked enumerator");
        assert_eq!(found.class, IdentityClass::MacAddress);
        // surrounding whitespace is not a different symbol
        assert!(runtime_symbol_for("  os.Hostname\n").is_some());
        // ...but the same name inside a run of concatenated literals is not a symbol
        assert!(runtime_symbol_for("dialing net.Interfaces failed: %w").is_none());
        assert!(runtime_symbol_for("net.Interfacesnet.InterfaceAddrs").is_none());
        // and an unrelated string is not one either
        assert!(runtime_symbol_for("net.Dial").is_none());
    }

    /// A GUID needs its RFC 4122 version and variant fields to be claimed.
    #[test]
    fn render_guid_requires_the_version_and_variant_fields() {
        // version 4, variant `10xxxxxx`
        let valid = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x48, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x00,
        ];
        assert_eq!(
            render_guid(&valid).as_deref(),
            Some("44332211-6655-4877-99aa-bbccddeeff00")
        );
        // version nibble 0 is not a defined UUID version
        let mut bad_version = valid;
        bad_version[7] = 0x07;
        assert!(render_guid(&bad_version).is_none());
        // variant bits `11xxxxxx` are not the RFC 4122 variant
        let mut bad_variant = valid;
        bad_variant[8] = 0xc0;
        assert!(render_guid(&bad_variant).is_none());
        // the unset sentinel identifies nothing
        assert!(render_guid(&[0u8; 16]).is_none());
    }

    /// The binary MAC filter rejects the degenerate runs that dominate raw data.
    #[test]
    fn render_mac_rejects_degenerate_runs() {
        assert_eq!(
            render_mac(&[0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e]).as_deref(),
            Some("00:1a:2b:3c:4d:5e")
        );
        // sentinels, fill patterns, and counters
        assert!(render_mac(&[0x00; 6]).is_none());
        assert!(render_mac(&[0xff; 6]).is_none());
        assert!(render_mac(&[0xcc; 6]).is_none());
        assert!(render_mac(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]).is_none());
        // an odd first octet is a group address, not an adapter's own
        assert!(render_mac(&[0x01, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e]).is_none());
        // an all-zero OUI is not an assigned manufacturer prefix
        assert!(render_mac(&[0x00, 0x00, 0x00, 0x3c, 0x4d, 0x5e]).is_none());
        // half zeros is a window over a table of small little-endian numbers, which
        // is what a pointer or offset table looks like six bytes at a time
        assert!(render_mac(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x00]).is_none());
        assert!(render_mac(&[0x00, 0x00, 0x5c, 0xb1, 0x02, 0x00]).is_none());
        assert!(render_mac(&[0x5c, 0xb1, 0x02, 0x00, 0x00, 0x00]).is_none());
    }

    /// One table does not become several addresses by being read at every offset.
    #[test]
    fn scan_window_does_not_report_shifted_copies() {
        let read: BTreeSet<IdentityClass> = [IdentityClass::MacAddress].into_iter().collect();
        // two real addresses back to back, so a sliding scan would find a third
        // straddling the pair
        let window = [
            0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x00, 0x0c, 0x29, 0x77, 0x88, 0x99,
        ];
        let found = scan_window(&window, &read);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].2, "00:1a:2b:3c:4d:5e");
        assert_eq!(found[1].2, "00:0c:29:77:88:99");
        // a table of 64-bit little-endian offsets yields nothing at all
        let table: Vec<u8> = [0x0002_b1b8_u64, 0x0002_b1a4, 0x0002_b18c, 0x0002_b174]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        assert!(scan_window(&table, &read).is_empty());
    }

    /// A window straddling two elements of an offset table is not an address.
    #[test]
    fn scan_window_rejects_a_slice_of_an_int_table() {
        let read: BTreeSet<IdentityClass> = [IdentityClass::MacAddress].into_iter().collect();
        // a table of 64-bit offsets with one flags word among them; the six bytes
        // spanning the flags word's tail and the next offset's head read as
        // `00:00:80:70:b3:02` — only two zeros, so the six-byte filters all pass
        let mut table: Vec<u8> = Vec::new();
        for value in [
            0x0002_b34c_u64,
            0x0002_b358,
            0x8000_0000_0000_009c,
            0x0002_b370,
        ] {
            table.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0x0002_b384_u64, 0x0002_b39c, 0x0002_b3b2] {
            table.extend_from_slice(&value.to_le_bytes());
        }
        // the six bytes on their own are indistinguishable from a real address...
        assert_eq!(
            render_mac(&table[21..27]).as_deref(),
            Some("00:00:80:70:b3:02")
        );
        // ...but in the company of the table they came from, they are a slice of it
        assert!(scan_window(&table, &read).is_empty());

        // an adapter address in an ordinary buffer is still claimed. The trailing
        // filler is one repeated byte so that it cannot itself render as an address
        // and consume the one under test
        let mut buffer = vec![0x00, 0x0c, 0x29, 0x77, 0x88, 0x99];
        buffer.extend_from_slice(&[0xcc; 24]);
        assert!(
            scan_window(&buffer, &read)
                .iter()
                .any(|(_, _, value)| value == "00:0c:29:77:88:99")
        );
    }

    /// A table of short strings is not a source of addresses, run by run.
    #[test]
    fn scan_window_rejects_slices_of_a_string_table() {
        let read: BTreeSet<IdentityClass> = [IdentityClass::MacAddress].into_iter().collect();
        // a command-line option table exactly as one sample stores it. No run in it
        // reaches `MAC_TEXT_RUN`, and the NULs land where the strings end rather than
        // on any stride, so neither of the other two neighbourhood tests sees it
        let table = b"y is 600 seconds.\n\0\0\0\0--help\0\0--sleep\0--delay\0[!] Inv";
        // the windows it yields are addresses by every test six bytes can apply...
        assert_eq!(
            render_mac(b"help\0\0").as_deref(),
            Some("68:65:6c:70:00:00")
        );
        assert_eq!(render_mac(b"delay\0").as_deref(), Some("64:65:6c:61:79:00"));
        // ...and none of them is one
        assert!(scan_window(table, &read).is_empty());

        // the same for a table of error strings, where the runs are longer but the
        // windows that straddle two of them still are not
        let errors = b"\0\0vector too long\0: \0\0\0\0\0\0generic\0invalid char fil";
        assert!(scan_window(errors, &read).is_empty());

        // an address stored at the head of an interface record keeps its own bytes.
        // The candidate is inside the neighbourhood it is judged on, so its own
        // high bytes put the region under the bar even though the rest is text.
        // Its octets are also chosen so none of them can serve as a GUID's version
        // nibble, which would otherwise let the sixteen-byte pass claim them first
        let mut buffer = vec![0x00, 0x0e, 0x0c, 0xb7, 0x88, 0xf9];
        buffer.extend_from_slice(b"\0eth0\0adapter name\0\0up\0\0\0\0\0");
        assert!(
            scan_window(&buffer, &read)
                .iter()
                .any(|(_, _, value)| value == "00:0e:0c:b7:88:f9")
        );
    }

    /// A GUID's bytes are not re-reported as a MAC address inside it.
    #[test]
    fn scan_window_lets_the_guid_claim_its_bytes() {
        let read: BTreeSet<IdentityClass> = [IdentityClass::Guid, IdentityClass::MacAddress]
            .into_iter()
            .collect();
        let window = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x48, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x02,
        ];
        let found = scan_window(&window, &read);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, IdentityClass::Guid);
        // with GUID enumeration absent the GUID is no longer worth reporting, but its
        // bytes are still its own — the six-byte scan does not get to carve addresses
        // out of a device class GUID just because nobody was seen reading one
        let mac_only: BTreeSet<IdentityClass> = [IdentityClass::MacAddress].into_iter().collect();
        assert!(scan_window(&window, &mac_only).is_empty());

        // the device class GUID a Windows program carries verbatim, laid out as it is
        // in memory and padded the way the compiler aligns it
        let class_guid = [
            0x67, 0xe9, 0x36, 0x4d, 0x25, 0xe3, 0xce, 0x11, 0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1,
            0x03, 0x18,
        ];
        let mut buffer = vec![0x00; 8];
        buffer.extend_from_slice(&class_guid);
        buffer.extend_from_slice(&[0x00; 8]);
        // read six bytes at a time it offers two entirely plausible addresses...
        assert_eq!(
            render_mac(&buffer[6..12]).as_deref(),
            Some("00:00:67:e9:36:4d")
        );
        assert_eq!(
            render_mac(&buffer[14..20]).as_deref(),
            Some("ce:11:bf:c1:08:00")
        );
        // ...and neither is claimed, because the GUID they are cut from claims them
        assert!(scan_window(&buffer, &mac_only).is_empty());
    }

    /// A six-byte window over a string is a piece of the string, not an address.
    #[test]
    fn scan_window_rejects_a_slice_of_a_text_run() {
        let read: BTreeSet<IdentityClass> = [IdentityClass::MacAddress].into_iter().collect();
        // two adjacent C strings, the shape a program's own name and one of its flag
        // words have in a data block. Every six-byte window over this passes the
        // filters `render_mac` can apply on its own — including the ones spanning the
        // terminators, which read as an address with a sparse leading OUI
        let mut window = b"al-khaser.exe\0\0\0CODE_INJECTIONS\0".to_vec();
        window.extend_from_slice(&[0x00; 8]);
        assert_eq!(
            render_mac(&window[1..7]).as_deref(),
            Some("6c:2d:6b:68:61:73")
        );
        assert_eq!(
            render_mac(&window[14..20]).as_deref(),
            Some("00:00:43:4f:44:45")
        );
        assert!(scan_window(&window, &read).is_empty());

        // an address that merely sits next to a string keeps its bytes: two of its
        // octets happen to be printable, which explains nothing, and the terminator
        // between it and the string keeps the two runs apart
        let mut buffer = vec![0x00, 0x0c, 0x29, 0x77, 0x88, 0x99];
        buffer.extend_from_slice(b"\0Checking VBox\0");
        buffer.extend_from_slice(&[0x00; 8]);
        assert!(!in_text_run(&buffer, 0));
        assert!(
            scan_window(&buffer, &read)
                .iter()
                .any(|(_, _, value)| value == "00:0c:29:77:88:99")
        );
        // ...while the window that starts on the terminator and runs into the string
        // is a piece of the string, however well it renders
        assert_eq!(
            render_mac(&buffer[6..12]).as_deref(),
            Some("00:43:68:65:63:6b")
        );
        assert!(in_text_run(&buffer, 6));
    }

    /// The comparison reader separates a guarded check from a bare mention.
    #[test]
    fn comparison_in_distinguishes_branches_from_mentions() {
        let guarded = "  if (strcmp(name, \"WIN-TARGET-01\") == 0) {\n    run();\n  }\n";
        assert_eq!(comparison_in(guarded, "WIN-TARGET-01"), Some(true));
        // a comparison whose result is stored rather than branched on
        let stored = "  iVar1 = strcmp(name, \"WIN-TARGET-01\");\n";
        assert_eq!(comparison_in(stored, "WIN-TARGET-01"), Some(false));
        // a literal that is only formatted, never tested
        let logged = "  sprintf(buf, \"host=%s\", \"WIN-TARGET-01\");\n";
        assert_eq!(comparison_in(logged, "WIN-TARGET-01"), None);
        // and a literal that does not appear at all
        assert_eq!(comparison_in(guarded, "OTHER-HOST"), None);
    }

    /// The cap is per class and shared across the passes that count against it.
    #[test]
    fn admits_caps_each_class_independently() {
        let mut counts: BTreeMap<IdentityClass, usize> = BTreeMap::new();
        // the whole allowance, taken by two passes counting against one tally
        for taken in 0..MAX_PER_CLASS {
            let source = if taken % 2 == 0 {
                "string table"
            } else {
                "literal pool"
            };
            assert!(admits(&mut counts, IdentityClass::MacAddress, source));
        }
        // saturated, and it stays saturated rather than resetting for a later pass
        assert!(!admits(
            &mut counts,
            IdentityClass::MacAddress,
            "literal pool"
        ));
        assert!(!admits(&mut counts, IdentityClass::MacAddress, "raw bytes"));
        // a different class has its own allowance
        assert!(admits(&mut counts, IdentityClass::Guid, "string table"));
    }

    /// A resource block holds packaged content, not runtime identity.
    #[test]
    fn asset_sections_are_excluded_from_the_binary_scan() {
        assert!(is_asset_section(".rsrc"));
        assert!(is_asset_section(".debug_info"));
        assert!(!is_asset_section(".data"));
        assert!(!is_asset_section(".rodata"));
    }

    /// The window read is clamped to the section it starts in.
    #[test]
    fn window_at_clamps_to_the_section_end() {
        let section = Section {
            name: ".data".to_string(),
            base: 0x40_1000,
            end: 0x40_1010,
            size: 16,
            permissions: "rw-".to_string(),
            is_initialized: true,
            is_loaded: true,
            bytes: (0..16u8).collect(),
            truncated: false,
        };
        let map = [section];
        let (found, window) = window_at(&map, 0x40_100c, 512).expect("in range");
        assert_eq!(found.name, ".data");
        assert_eq!(window, &[12, 13, 14, 15]);
        // an address past the block has no window
        assert!(window_at(&map, 0x40_2000, 512).is_none());
    }

    /// A code block is read as instructions, not as stored fields.
    #[test]
    fn executable_sections_are_excluded_from_the_binary_scan() {
        // the shape the scan is for: a field written into a data block
        assert!(holds_stored_data(&scan_section(".data", "rw-")));
        assert!(holds_stored_data(&scan_section(".rodata", "r--")));
        // a code block, whose six-byte windows are opcodes and operands
        assert!(!holds_stored_data(&scan_section(".text", "r-x")));
        // and the writable code block a runtime-obfuscated sample carries, which
        // holds a compressed stream until it is written to
        assert!(!holds_stored_data(&scan_section("UPX1", "rwx")));
    }

    /// Build an enumerating-function fixture reading one class.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function's entry address
    /// * `class` - The identity class it was seen reading
    /// * `label` - How the enumerator is described
    fn enumerator(entry: u64, class: IdentityClass, label: &'static str) -> EnumeratingFunction {
        EnumeratingFunction {
            entry,
            classes: [class].into_iter().collect(),
            labels: [label].into_iter().collect(),
            sites: Vec::new(),
        }
    }

    /// A block holding one stored address, with every reference reaching it.
    ///
    /// The address leads the block so nothing precedes it to be sliced with, and the
    /// filler behind it is one repeated byte, which [`render_mac`] rejects outright —
    /// so the block holds exactly one candidate and a test over it is measuring the
    /// bookkeeping rather than the scan.
    fn stored_value_section() -> Section {
        let mut bytes = vec![0x08, 0x00, 0x27, 0xaa, 0xbb, 0xcc];
        bytes.resize(0x40, 0xaa);
        Section {
            name: ".data".to_string(),
            base: 0x40_1000,
            end: 0x40_1040,
            size: 0x40,
            permissions: "rw-".to_string(),
            is_initialized: true,
            is_loaded: true,
            bytes,
            truncated: false,
        }
    }

    /// One stored value is one finding, however many functions reach it.
    #[test]
    fn collect_stored_values_merges_the_functions_reaching_one_value() {
        let sections = vec![stored_value_section()];
        let enumerators: HashMap<u64, EnumeratingFunction> = [
            (
                0x1000,
                enumerator(0x1000, IdentityClass::MacAddress, "GetAdaptersInfo"),
            ),
            (
                0x2000,
                enumerator(0x2000, IdentityClass::MacAddress, "GetAdaptersAddresses"),
            ),
        ]
        .into_iter()
        .collect();
        // both functions reference the buffer, and one references it twice
        let index = [
            (0x40_1000, 0x2000),
            (0x40_1000, 0x1000),
            (0x40_1000, 0x1000),
        ];
        let found = collect_stored_values(&sections, &index, &enumerators);
        assert_eq!(found.len(), 1);
        let ((class, at, value), (section, users)) = found.into_iter().next().unwrap();
        assert_eq!(class, IdentityClass::MacAddress);
        assert_eq!(at, 0x40_1000);
        assert_eq!(value, "08:00:27:aa:bb:cc");
        assert_eq!(section.name, ".data");
        // the second function is more attribution for the one value, not a second
        // finding, and the repeated reference is not a third
        assert_eq!(users, vec![0x2000, 0x1000]);
    }

    /// A function reading a different class does not justify the scan.
    #[test]
    fn collect_stored_values_scopes_the_scan_to_what_the_function_reads() {
        let sections = vec![stored_value_section()];
        // this function reads the account name, so six bytes in the data it
        // references are six bytes, not a hardware address
        let enumerators: HashMap<u64, EnumeratingFunction> = [(
            0x3000,
            enumerator(0x3000, IdentityClass::AccountName, "GetUserName"),
        )]
        .into_iter()
        .collect();
        let index = [(0x40_1000, 0x3000)];
        assert!(collect_stored_values(&sections, &index, &enumerators).is_empty());

        // and it stays out of the attribution when another function does read one
        let mut enumerators = enumerators;
        enumerators.insert(
            0x1000,
            enumerator(0x1000, IdentityClass::MacAddress, "GetAdaptersInfo"),
        );
        let index = [(0x40_1000, 0x3000), (0x40_1000, 0x1000)];
        let found = collect_stored_values(&sections, &index, &enumerators);
        assert_eq!(found.len(), 1);
        assert_eq!(found.into_values().next().unwrap().1, vec![0x1000]);
    }

    /// Build a section fixture carrying only what the scan gates read.
    ///
    /// # Arguments
    ///
    /// * `name` - The section name
    /// * `permissions` - The section's permission string
    fn scan_section(name: &str, permissions: &str) -> Section {
        Section {
            name: name.to_string(),
            base: 0x40_1000,
            end: 0x40_1010,
            size: 16,
            permissions: permissions.to_string(),
            is_initialized: true,
            is_loaded: true,
            bytes: (0..16u8).collect(),
            truncated: false,
        }
    }

    /// A candidate sharing bytes with a stored pointer is a slice of the table.
    #[test]
    fn straddles_pointer_rejects_slices_of_a_pointer_table() {
        // the shape a C++ runtime's type descriptors have, which is the densest
        // pointer table a compiled binary carries: a pointer to the shared vftable,
        // a null spare, then the mangled type name, repeating — so the boundary
        // between plain text and a pointer recurs every few bytes
        let mut bytes = Vec::new();
        // 0x140044870: a pointer into the read-only block
        bytes.extend_from_slice(&0x1_4002_11c0u64.to_le_bytes());
        // 0x140044878: a small integer field
        bytes.extend_from_slice(&3u64.to_le_bytes());
        // 0x140044880: the first descriptor's vftable pointer
        bytes.extend_from_slice(&0x1_4001_cfe8u64.to_le_bytes());
        // 0x140044888: its null spare
        bytes.extend_from_slice(&[0u8; 8]);
        // 0x140044890: its mangled name, thirty bytes, then two of padding
        bytes.extend_from_slice(b".?AVbad_array_new_length@std@@\0\0");
        // 0x1400448b0: the second descriptor, immediately after
        bytes.extend_from_slice(&0x1_4001_cfe8u64.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        // 0x1400448c0: a hardware address stored in its own eight-byte slot
        bytes.extend_from_slice(&[0x00, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x00, 0x00]);
        // 0x1400448c8: the field after it
        bytes.extend_from_slice(&1u64.to_le_bytes());
        let data = Section {
            name: ".data".to_string(),
            base: 0x1_4004_4870,
            end: 0x1_4004_48d0,
            size: 0x60,
            permissions: "rw-".to_string(),
            is_initialized: true,
            is_loaded: true,
            bytes,
            truncated: false,
        };
        assert_eq!(data.bytes.len(), 0x60);
        // the block the pointers point into, which is what makes them pointers
        let rdata = Section {
            name: ".rdata".to_string(),
            base: 0x1_4001_a000,
            end: 0x1_4002_c000,
            size: 0x1_2000,
            permissions: "r--".to_string(),
            is_initialized: true,
            is_loaded: true,
            bytes: Vec::new(),
            truncated: false,
        };
        let map = [rdata, data.clone()];
        // a window spanning the spare's trailing zeros and the descriptor's pointer
        // renders as an address, and is six bytes of two different fields
        assert_eq!(
            render_mac(&data.bytes[0x0e..0x14]).as_deref(),
            Some("00:00:e8:cf:01:40")
        );
        assert!(straddles_pointer(&map, &data, 0x1_4004_487e, 6));
        // and so does one spanning the end of a mangled name and the pointer that
        // opens the next descriptor
        assert_eq!(
            render_mac(&data.bytes[0x3c..0x42]).as_deref(),
            Some("40:40:00:00:e8:cf")
        );
        assert!(straddles_pointer(&map, &data, 0x1_4004_48ac, 6));
        // the stored address in its own slot shares no byte with a pointer
        assert_eq!(
            render_mac(&data.bytes[0x50..0x56]).as_deref(),
            Some("00:1a:2b:3c:4d:5e")
        );
        assert!(!straddles_pointer(&map, &data, 0x1_4004_48c0, 6));
        // and a word decoding to an address outside every loaded block is a number,
        // so the descriptor's integer field claims nothing
        assert!(!straddles_pointer(&map, &data, 0x1_4004_4878, 6));
    }
}
