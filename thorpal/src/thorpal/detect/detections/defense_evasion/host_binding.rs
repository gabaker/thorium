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
//! 3. **Binary identifiers** — sixteen-byte GUID values stored as bytes rather
//!    than text, searched **only** in the buffers that enumerating functions
//!    reference. The GUID is the only shape searched at the byte level: its
//!    version and variant fields are checkable structure, where six raw MAC
//!    bytes have none — a byte-level MAC search fabricates addresses out of
//!    ordinary compiler output (format strings, C++ metadata, security cookies,
//!    import tables) faster than neighbourhood tests can reject them, so a
//!    stored MAC is only ever recovered from a text rendering.
//! 4. **Pooled literals** — the same rendered shapes as (1), but read out of the
//!    raw bytes of the data blocks, so that a value in a concatenated literal
//!    pool the string finder never split into strings is still recovered.
//!
//! # Why the enumerator matters so much here
//!
//! Every one of these shapes is cheap to match and expensive to be wrong about.
//! A rendered identifier also appears in documentation; an `XXXX-XXXX` hex run
//! is also a part number; a short identifier is also a registry subkey. What
//! separates an environmental key from a coincidence is not the value — it is
//! that the program *asks the operating system for the same class of value* and
//! then compares. So a literal with no observed comparison is reported at
//! [`low`] and [`EvidenceKind::ContextOnly`] whether or not an enumerating
//! function references it — co-location is attribution, not evidence of keying —
//! and only an observed comparison feeding a branch earns [`medium`] and
//! [`EvidenceKind::ControlFlowGated`], which is what lets `validate.rs` confirm
//! it. That comparison tier is also the only one that claims **T1480.001** at
//! all; everything short of it is reported as **T1082** identity context (see
//! [`score_identity`]). Nothing here is ever *emitted* at `Confidence::Fact` — a shape
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

use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::context::{AnalysisContext, Section, fmt_addr, parse_addr};
use crate::thorpal::detect::decode::buffers::referencing_function;
use crate::thorpal::detect::detections::{
    ArtifactClass, AttackTechnique, Detection, DetectionHit, EvidenceKind, HitArtifact,
    is_library_function, low, medium, normalize,
};
use crate::thorpal::detect::indicators::{
    HostIdKind, HostIdMatch, find_blob_host_identifiers, find_host_identifiers, is_ubiquitous_guid,
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
/// points at, not at the whole image. A structure holding a machine GUID sits
/// within a few hundred bytes of the pointer that reaches it.
const BINARY_SCAN_WINDOW: usize = 512;

/// How many bytes a GUID occupies when stored as raw data.
const GUID_BYTE_LEN: u64 = 16;

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

/// The most GUID literal findings reported when nothing reads a machine GUID.
///
/// Without an observed GUID enumerator, GUID-shaped literals are overwhelmingly
/// interface identifiers, resource references, and toolchain constants, so only
/// a handful are worth a finding each; the rest fold into one summary line (see
/// [`guid_overflow_hit`]). The allowance is deliberately above the one-to-three
/// unique campaign GUIDs a keyed sample actually carries.
const MAX_UNATTRIBUTED_GUIDS: usize = 6;

/// The most name candidates reported from any single enumerating function.
///
/// Candidates are already comparison-gated (see [`name_candidate_hits`]), so a
/// function legitimately checking a name list needs only a few shown
/// individually; anything past the cap folds into one summary line.
const MAX_NAMES_PER_FUNCTION: usize = 3;

/// The most overflowed values a summary line spells out before counting the rest.
const MAX_LISTED_OVERFLOW: usize = 12;

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
    /// Whether this function *is* a linked runtime's own routine.
    ///
    /// Set when a [`RUNTIME_ENUMERATORS`] marker matched the function's name: the
    /// marker names the runtime's wrapper itself (`os.Hostname`, `whoami::`), not
    /// a caller of it, so the function's data references are the runtime reaching
    /// its own tables. A wrapper still anchors the enumeration finding — the read
    /// is real — but it never anchors a stored-value scan, a name candidate, or
    /// the attribution of a recovered value (see [`anchor_for`]).
    runtime_wrapper: bool,
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
        hits.extend(enumeration_hits(
            |entry| ctx.function_name(entry).map(str::to_string),
            &enumerators,
        ));
        // ...and the same read where stripping left only the runtime's own name table
        hits.extend(runtime_symbol_hits(ctx, &enumerators));
        // whether anything reads a machine GUID: without that read, GUID literals
        // fall under the tighter unattributed allowance
        let guid_enumerated = enumerators
            .values()
            .any(|record| record.classes.contains(&IdentityClass::Guid));
        // the two text passes search the same bytes by different routes, so they share
        // a cap tally, a set of claimed addresses (a value the string table already
        // yielded must not be reported a second time by the raw scan), and the list of
        // unattributed GUIDs their caps dropped
        let mut counts: BTreeMap<IdentityClass, usize> = BTreeMap::new();
        let mut claimed: BTreeSet<u64> = BTreeSet::new();
        let mut guid_overflow: Vec<(String, u64)> = Vec::new();
        // 1) identity values rendered as text in the string table
        hits.extend(
            literal_hits(
                ctx,
                &enumerators,
                &mut counts,
                &mut claimed,
                guid_enumerated,
                &mut guid_overflow,
            )
            .await?,
        );
        // 2) name literals, which have no shape and are only candidates because an
        //    enumerating function references them
        hits.extend(name_candidate_hits(ctx, &enumerators).await?);
        // 3) GUIDs stored as raw bytes in the buffers those functions reach
        hits.extend(binary_hits(ctx, &enumerators).await?);
        // 4) ...and the text renderings the string finder could not see, because the
        //    literal they sit in was never terminated
        hits.extend(
            blob_literal_hits(
                ctx,
                &enumerators,
                &mut counts,
                &claimed,
                guid_enumerated,
                &mut guid_overflow,
            )
            .await?,
        );
        // the GUIDs the unattributed allowance dropped, as one visible summary line
        // rather than a silent truncation
        hits.extend(guid_overflow_hit(&guid_overflow));
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
                runtime_wrapper: false,
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
            runtime_wrapper: false,
        });
        record.classes.insert(marker.class);
        record.labels.insert(marker.label);
        // the marker names the runtime's own routine, however it was first found
        record.runtime_wrapper = true;
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
/// Functions whose recovered name marks recognized runtime/library code (glibc,
/// the loader — see [`is_library_function`]) are skipped outright: the runtime
/// reads identity for its own housekeeping, not as the sample's behavior.
///
/// # Arguments
///
/// * `name_of` - Resolves a function entry to its recovered name, when one exists
/// * `enumerators` - The functions found reading machine identity
fn enumeration_hits(
    name_of: impl Fn(u64) -> Option<String>,
    enumerators: &HashMap<u64, EnumeratingFunction>,
) -> Vec<DetectionHit> {
    // stable ordering so the report does not churn between runs
    let mut ordered: Vec<&EnumeratingFunction> = enumerators.values().collect();
    ordered.sort_by_key(|record| record.entry);
    ordered
        .into_iter()
        .filter_map(|record| {
            // resolve the recovered name once: it both gates and labels the hit
            let name = name_of(record.entry);
            // a recognized runtime/library routine (glibc, the loader) reads
            // identity for its own housekeeping, never as the sample's behavior
            if name.as_deref().is_some_and(is_library_function) {
                return None;
            }
            // anchor at the first call site when there is one, else the function
            let address = record.sites.first().copied().unwrap_or(record.entry);
            let named = name.unwrap_or_else(|| fmt_addr(record.entry));
            Some(
                DetectionHit::new(
                    T1082,
                    address,
                    Some(record.entry),
                    low(),
                    format!("Reads machine identity in {named}: {}", record.describe()),
                    format!(
                        "`{named}` asks the operating system for machine-identifying values via \
                         {}. Reading these is ordinary by itself — licensing, telemetry, \
                         clustering, and crash reporting all do it — but it is the read that \
                         makes any stored value of the same class in this program a candidate \
                         environmental key.",
                        record.describe(),
                    ),
                )
                .kind(EvidenceKind::ContextOnly),
            )
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

/// Whether another finding of `class` fits under `cap`.
///
/// Counts the finding when it does, so a caller only has to ask. The first value a
/// class drops is logged, once, because a silent truncation reads as "that was all
/// of them".
///
/// # Arguments
///
/// * `counts` - The per-class tally, updated in place
/// * `class` - The class the candidate belongs to
/// * `cap` - The most individual findings this class may report
/// * `source` - What the tally covers, named in the warning
fn admits(
    counts: &mut BTreeMap<IdentityClass, usize>,
    class: IdentityClass,
    cap: usize,
    source: &str,
) -> bool {
    let count = counts.entry(class).or_default();
    // under the cap: take the finding and charge it to the class
    if *count < cap {
        *count += 1;
        return true;
    }
    // exactly at the cap: this is the first value being dropped, so say so and step
    // one past the cap to keep the rest of the class quiet
    if *count == cap {
        tracing::warn!(
            class = class.label(),
            cap,
            source,
            "host-identity cap reached; further values of this class are left out of the \
             per-value findings"
        );
        *count += 1;
    }
    false
}

/// Recover MAC, GUID, and volume-serial literals and attribute each to a function.
///
/// GUIDs carry two pieces of flood control the rarer shapes do not need: the
/// ubiquitous platform identifiers are dropped outright (see
/// [`is_ubiquitous_guid`]), and the GUIDs found inside one defined string are
/// reported as one finding listing them all rather than one finding each — a
/// string holding several is a stored table, and each sibling is more of the one
/// fact, not another fact.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `enumerators` - The functions found reading machine identity
/// * `counts` - The per-class cap tally, shared with [`blob_literal_hits`]
/// * `claimed` - The addresses reported here, recorded for [`blob_literal_hits`]
/// * `guid_enumerated` - Whether anything was observed reading a machine GUID
/// * `guid_overflow` - The unattributed GUIDs dropped by the cap, for the summary
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
    guid_enumerated: bool,
    guid_overflow: &mut Vec<(String, u64)>,
) -> Result<Vec<DetectionHit>> {
    // the GUID allowance: with no GUID read observed anywhere, far fewer GUID
    // literals are worth a finding each
    let guid_cap = if guid_enumerated {
        MAX_PER_CLASS
    } else {
        MAX_UNATTRIBUTED_GUIDS
    };
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
        // record where every value sits before any filter or cap can drop it, so
        // the raw scans stay quiet about these bytes either way
        for found in &matches {
            claimed.insert(base.saturating_add(found.at as u64));
        }
        // group the string's reportable GUIDs; the other shapes pass through
        let (guids, others) = split_string_matches(matches);
        // MAC and serial literals: one finding per value, since the shapes are rare
        for found in others {
            let class = IdentityClass::from_shape(found.kind);
            // honor the per-class cap, logging the first value dropped
            if !admits(counts, class, MAX_PER_CLASS, "string table") {
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
        // GUIDs: one finding for the whole string, however many it holds
        if guids.is_empty() {
            continue;
        }
        if !admits(counts, IdentityClass::Guid, guid_cap, "string table") {
            // the summary line stands in for the values the unattributed cap drops
            if !guid_enumerated {
                guid_overflow.extend(guids.into_iter().map(|found| (found.value, base)));
            }
            continue;
        }
        // find the functions that use this literal, preferring real xrefs
        let users = literal_users(ctx, &string.address, base, DATA_REF_WINDOW, &mut index).await?;
        // the origin names every sibling GUID, so the one finding lists them all;
        // scoring reads the first value, which costs nothing real — a string
        // holding several GUIDs is a stored table, not a compared key
        let origin = if guids.len() == 1 {
            format!("string literal at {}", fmt_addr(base))
        } else {
            format!(
                "string literal at {} holding {} GUIDs: {}",
                fmt_addr(base),
                guids.len(),
                guids
                    .iter()
                    .map(|found| format!("`{}`", found.value))
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        hits.push(
            build_identity_hit(
                ctx,
                enumerators,
                IdentityClass::Guid,
                &guids[0].value,
                base,
                &users,
                &origin,
            )
            .await?,
        );
    }
    Ok(hits)
}

/// Split one defined string's identity matches into reportable GUIDs and the rest.
///
/// The GUIDs come back as one group — several GUIDs inside one defined string are
/// one stored table, reported as one finding — with the ubiquitous platform
/// identifiers already dropped; the other shapes pass through untouched for their
/// per-value findings.
///
/// # Arguments
///
/// * `matches` - Every identity literal found in the string, in text order
fn split_string_matches(matches: Vec<HostIdMatch>) -> (Vec<HostIdMatch>, Vec<HostIdMatch>) {
    // split the GUIDs from the other shapes
    let (guids, others): (Vec<_>, Vec<_>) = matches
        .into_iter()
        .partition(|found| found.kind == HostIdKind::Guid);
    // drop the interface and class identifiers every COM/DirectX client carries
    let guids = guids
        .into_iter()
        .filter(|found| !is_ubiquitous_guid(&found.value))
        .collect();
    (guids, others)
}

/// Render a bounded, backtick-quoted list of overflowed values.
///
/// # Arguments
///
/// * `values` - The values to list, in report order
fn list_values<'a>(values: impl ExactSizeIterator<Item = &'a str>) -> String {
    // how many are left unspelled once the listing is bounded
    let rest = values.len().saturating_sub(MAX_LISTED_OVERFLOW);
    // quote the bounded prefix
    let listed = values
        .take(MAX_LISTED_OVERFLOW)
        .map(|value| format!("`{value}`"))
        .collect::<Vec<_>>()
        .join(", ");
    // count the tail rather than spelling it, so the summary stays one line
    if rest > 0 {
        format!("{listed} and {rest} more")
    } else {
        listed
    }
}

/// One summary finding for the unattributed GUIDs past the reporting cap.
///
/// Emitted only when nothing in the program was observed reading a machine GUID
/// (see [`MAX_UNATTRIBUTED_GUIDS`]): the dropped values are named in one line so
/// the truncation is visible without each value spending a finding of its own.
/// Returns `None` when nothing was dropped.
///
/// # Arguments
///
/// * `overflow` - The dropped `(value, address)` pairs, in recovery order
fn guid_overflow_hit(overflow: &[(String, u64)]) -> Option<DetectionHit> {
    // the first dropped value anchors the summary; none dropped means no summary
    let (_, address) = overflow.first()?;
    // spell the values out, bounded so a flooded binary stays one line
    let listed = list_values(overflow.iter().map(|(value, _)| value.as_str()));
    Some(
        DetectionHit::new(
            T1082,
            *address,
            None,
            low(),
            format!(
                "{} further GUID literals beyond the unattributed reporting cap",
                overflow.len()
            ),
            format!(
                "Nothing in this program was observed reading a machine GUID, so GUID-shaped \
                 literals are reported individually only up to a cap of \
                 {MAX_UNATTRIBUTED_GUIDS}. {} more were recovered past it: {listed}. Without an \
                 enumerator these are overwhelmingly interface identifiers, resource references, \
                 and toolchain constants rather than environmental keys.",
                overflow.len(),
            ),
        )
        .kind(EvidenceKind::ContextOnly),
    )
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
/// filter would accept most short identifiers in the binary — and even inside
/// one, a candidate is only reported when the function's decompilation shows the
/// literal in a comparison. Mere co-location means nothing here: an enumerating
/// function references its log fields, registry value names, and decoy pools
/// just as readily as a key it checks, and only the comparison separates them.
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
        // nor is a linked runtime's own wrapper, which references the runtime's
        // literals as a matter of course
        if record.runtime_wrapper {
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
        // keep only the candidates the decompilation shows in a comparison — a
        // referenced-but-never-compared literal is a log field or a decoy, not an
        // expected value. The decompile is memoized, so this costs no bridge
        // traffic beyond what `build_identity_hit` fetches for scoring anyway
        let mut gated: Vec<NameCandidate> = Vec::new();
        for value in candidates {
            if observed_comparison(ctx, record.entry, &value.text)
                .await
                .is_some()
            {
                gated.push(value);
            }
        }
        // report the first few individually; the rest fold into one summary line
        let overflow = gated.split_off(gated.len().min(MAX_NAMES_PER_FUNCTION));
        for value in &gated {
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
        // the candidates past the cap, as one visible line instead of a silent drop
        let named = ctx
            .function_name(record.entry)
            .map_or_else(|| fmt_addr(record.entry), str::to_string);
        hits.extend(name_overflow_hit(&named, record.entry, &overflow));
    }
    Ok(hits)
}

/// One summary finding for a function's compared name candidates past the cap.
///
/// Returns `None` when nothing overflowed.
///
/// # Arguments
///
/// * `named` - The enumerating function's display name
/// * `entry` - The enumerating function's entry address
/// * `overflow` - The gated candidates past [`MAX_NAMES_PER_FUNCTION`]
fn name_overflow_hit(named: &str, entry: u64, overflow: &[NameCandidate]) -> Option<DetectionHit> {
    // nothing overflowed, so there is nothing to summarize
    if overflow.is_empty() {
        return None;
    }
    // spell the values out, bounded so a pathological function stays one line
    let listed = list_values(overflow.iter().map(|candidate| candidate.text.as_str()));
    Some(
        DetectionHit::new(
            T1082,
            entry,
            Some(entry),
            low(),
            format!(
                "{} further stored name candidates compared in {named}",
                overflow.len()
            ),
            format!(
                "`{named}` reads a machine or account name and its decompilation shows {} \
                 further name-shaped literals in comparisons beyond the \
                 {MAX_NAMES_PER_FUNCTION} reported individually: {listed}. Each carries the \
                 same evidence shape as the reported candidates and can be checked the same \
                 way.",
                overflow.len(),
            ),
        )
        .kind(EvidenceKind::ContextOnly),
    )
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
        // the inputs of enumeration itself are never the expected value
        if is_enumeration_input(string.value.trim()) {
            continue;
        }
        candidates.push(NameCandidate {
            address: addr,
            text: string.value.trim().to_string(),
        });
    }
    Ok(candidates)
}

/// The literals that are enumeration *inputs* rather than candidate keys.
///
/// A function that reads host identity through the registry or the resolver
/// references these names in order to ask the question — `RegQueryValueExA(h,
/// "MachineGuid", …)`, glibc consulting `RES_OPTIONS` — so one of them next to an
/// enumerator is the enumeration working as designed, not a stored expected value.
const ENUMERATION_INPUTS: &[&str] = &[
    // the registry value names Windows identity and licensing data is read by
    "InstallDate",
    "RegisteredOwner",
    "RegisteredOrganization",
    "MachineGuid",
    "DigitalProductId",
    "ProductId",
    // the environment variables the libc resolver consults
    "RES_OPTIONS",
    "HOSTALIASES",
    "LOCALDOMAIN",
];

/// Whether a name literal is one of the enumeration-input words.
///
/// # Arguments
///
/// * `value` - The trimmed literal to test
fn is_enumeration_input(value: &str) -> bool {
    // registry value names compare case-insensitively, so match them that way
    ENUMERATION_INPUTS
        .iter()
        .any(|input| value.eq_ignore_ascii_case(input))
}

// ===========================================================================
// 3) Binary identifiers in the buffers enumerators reach
// ===========================================================================

/// Recover sixteen-byte GUID values stored as raw bytes.
///
/// Scoped hard: only the buffers an enumerating function references are read,
/// and only when the program enumerates a machine GUID at all — an unscoped
/// sixteen-byte scan matches every compiled-in interface identifier a COM client
/// carries. The GUID is also the only shape searched at the byte level. Six raw
/// MAC bytes carry no verifiable structure, so scanning referenced buffers for
/// them fabricates addresses out of format strings, C++ RTTI metadata, MSVC
/// security cookies, and import tables faster than neighbourhood tests can
/// reject them — stored MACs are recovered only from their text renderings (the
/// string-table and literal-pool passes).
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
    // only an observed machine-GUID read justifies the byte-level search
    if !enumerators
        .values()
        .any(|record| record.classes.contains(&IdentityClass::Guid))
    {
        return Ok(Vec::new());
    }
    let sections = ctx.sections().await?;
    let index = ctx.reference_index().await?;
    let mut hits = Vec::new();
    let mut counts: BTreeMap<IdentityClass, usize> = BTreeMap::new();
    for ((at, value), (section, users)) in collect_stored_values(&sections, &index, enumerators) {
        // honor the per-class cap, logging the first value dropped
        if !admits(&mut counts, IdentityClass::Guid, MAX_PER_CLASS, "raw bytes") {
            continue;
        }
        hits.push(
            build_identity_hit(
                ctx,
                enumerators,
                IdentityClass::Guid,
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

/// Find the stored GUIDs the enumerating functions reach.
///
/// Returns one entry per value, keyed by where it is, holding the block it was
/// found in and every enumerating function that reaches it.
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
/// than to what the program reads somewhere. The only thing that makes sixteen
/// bytes a stored machine GUID rather than an interface identifier is that they
/// sit in a buffer belonging to a routine that handles machine GUIDs, so a routine
/// that reads the account name lends the pattern nothing, and letting it do so
/// both invents values and credits them to a function that never touched one. A
/// linked runtime's own wrapper is excluded for the same reason: its references
/// are the runtime reaching its own tables, not the author's data.
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
) -> BTreeMap<(u64, String), (&'a Section, Vec<u64>)> {
    let mut candidates: BTreeMap<(u64, String), (&Section, Vec<u64>)> = BTreeMap::new();
    // walk only the data addresses the enumerating functions reference
    for (addr, entry) in index {
        let Some(record) = enumerators.get(entry) else {
            continue;
        };
        // only a function seen reading a machine GUID scopes the GUID scan
        if !record.classes.contains(&IdentityClass::Guid) {
            continue;
        }
        // a runtime's own wrapper references the runtime's data, never the author's
        if record.runtime_wrapper {
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
        for (offset, value) in scan_window(window) {
            let at = addr.saturating_add(offset as u64);
            // a candidate sharing bytes with a stored pointer is a slice of a pointer
            // table, not a value — this is judged against the whole block rather than
            // the window so a candidate at the window's edge is still judged
            if straddles_pointer(sections, section, at, GUID_BYTE_LEN) {
                continue;
            }
            let users = &mut candidates
                .entry((at, value))
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
/// A byte scan needs a haystack where consecutive bytes plausibly form one value.
/// Data blocks are such a haystack: a stored identifier is written as a field and
/// read back as a field, so a match is aligned with something the program treats as
/// a unit. Instruction bytes are not. A code block is a dense stream in which the
/// byte boundaries belong to opcodes and operands, so a shape matched across it is
/// a view of whatever the encoding happens to allow, not a field.
///
/// Executability is the test rather than a name list because it is the property that
/// actually matters and it holds across formats; a scan restricted to blocks Ghidra
/// says are not executed cannot read code as data whatever the block is called. Note
/// this also excludes the writable-and-executable block a runtime-obfuscated sample
/// carries, which is correct for a different reason: before it is written to it holds
/// a compressed or encrypted stream, and a shaped pattern found in high-entropy
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
/// [`render_guid`] asks what a candidate's own bytes are; this asks a different and
/// sharper question about the bytes around them, and gets a definite answer: a byte
/// that belongs to a stored address cannot also belong to a stored identifier, so a
/// candidate overlapping one is not a value, it is a window that happens to span the
/// end of one field and the start of the next.
///
/// The reason it matters is that pointer tables are where byte-scan fabrications
/// come from, and every compiled binary is full of them: vtables, relocated data
/// pointers, jump tables, and — the densest source, because each of its entries is a
/// pointer followed by a mangled type name, so the boundary between plain text and a
/// pointer recurs every few bytes — a C++ runtime's type-descriptor table.
///
/// A word counts only when it is aligned to its own width and decodes to an address
/// inside a loaded block, which is what keeps this from rejecting real values: for
/// arbitrary bytes to be read as part of a pointer, the bytes beside them have to
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

/// Find binary GUID values inside a window of bytes.
///
/// Returns `(offset, rendered value)` per match. The GUID is the only shape this
/// scan looks for: its RFC 4122 version and variant fields are structure
/// [`render_guid`] can verify, where six raw MAC bytes have nothing to check —
/// any six-byte scan over real data fabricates addresses out of format strings,
/// C++ RTTI/EH metadata, MSVC security cookies, and import tables, so stored
/// MACs are recovered only from their separator-bearing text renderings.
///
/// # Arguments
///
/// * `window` - The bytes to scan
fn scan_window(window: &[u8]) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    // step over a match rather than past it by one: two GUIDs cannot overlap, so
    // a byte already inside one is not the start of another
    let mut offset = 0;
    while offset + 16 <= window.len() {
        // claim the sixteen bytes when they verify as a GUID
        if let Some(value) = render_guid(&window[offset..offset + 16]) {
            found.push((offset, value));
            offset += 16;
            continue;
        }
        offset += 1;
    }
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
/// because raw bytes on their own prove nothing. A rendered value is a different
/// proposition — a fixed grammar with separators, with the degenerate renderings
/// already filtered — so the shape carries its own weight and a hit with no
/// enumerator behind it is simply reported at [`low`], exactly as the
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
/// * `guid_enumerated` - Whether anything was observed reading a machine GUID
/// * `guid_overflow` - The unattributed GUIDs dropped by the cap, for the summary
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
    guid_enumerated: bool,
    guid_overflow: &mut Vec<(String, u64)>,
) -> Result<Vec<DetectionHit>> {
    // the GUID allowance, matching `literal_hits` so the shared tally stays coherent
    let guid_cap = if guid_enumerated {
        MAX_PER_CLASS
    } else {
        MAX_UNATTRIBUTED_GUIDS
    };
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
            // the interface and class identifiers every COM/DirectX client carries
            if class == IdentityClass::Guid && is_ubiquitous_guid(&found.value) {
                continue;
            }
            // honor the per-class cap, logging the first value dropped; GUIDs run
            // under the unattributed allowance when nothing reads one
            let cap = if class == IdentityClass::Guid {
                guid_cap
            } else {
                MAX_PER_CLASS
            };
            if !admits(counts, class, cap, "literal pool") {
                // the summary line stands in for the values the unattributed cap drops
                if class == IdentityClass::Guid && !guid_enumerated {
                    guid_overflow.push((found.value, at));
                }
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
/// shape sets nothing but the class, co-location with an enumerating function
/// sets nothing but the attribution, and only an observed comparison earns the
/// environmental-keying claim — [`score_identity`] is the one place that ladder
/// is written down.
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
    // the strongest attribution available, never a runtime's own wrapper
    let matched = anchor_for(users, enumerators, class);
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
    let (technique, severity, kind) = score_identity(matched.is_some(), comparison);
    // the headline: the value, its class, and the strongest observed relationship —
    // a keying phrase only where the comparison was actually seen
    let content = match (matched, comparison) {
        (Some(record), Some(true)) => format!(
            "Stored {} `{value}` compared in a branch by {named}, which reads the machine's own \
             value via {}",
            class.label(),
            record.describe(),
        ),
        (Some(record), _) => format!(
            "Stored {} `{value}` co-located with the identity read in {named} ({})",
            class.label(),
            record.describe(),
        ),
        (None, _) => format!("Stored {} `{value}`", class.label()),
    };
    // a runtime wrapper among the users reads identity, so the unattributed
    // reasoning must talk about whose data this is rather than deny the read
    let wrapper_user = users
        .iter()
        .filter_map(|entry| enumerators.get(entry))
        .any(|record| record.runtime_wrapper);
    // the reasoning: what was recovered, what corroborates it, and what does not
    let reasoning = identity_reasoning(class, origin, &named, matched, comparison, wrapper_user);
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
        DetectionHit::new(technique, address, primary, severity, content, reasoning)
            .kind(kind)
            .artifact(artifact),
    )
}

/// Compose the reasoning for one host-identity finding.
///
/// Split out of [`build_identity_hit`] so the evidence prose sits in one place:
/// each arm states exactly what was observed and claims nothing past it.
///
/// # Arguments
///
/// * `class` - What the value identifies
/// * `origin` - A phrase describing where the value was recovered from
/// * `named` - The attributed function's display name, empty when none exists
/// * `matched` - The enumerating function anchoring the value, when one does
/// * `comparison` - What the decompilation showed for the value
/// * `wrapper_user` - Whether a runtime's own wrapper is among the value's users
fn identity_reasoning(
    class: IdentityClass,
    origin: &str,
    named: &str,
    matched: Option<&EnumeratingFunction>,
    comparison: Option<bool>,
    wrapper_user: bool,
) -> String {
    // open with what was recovered and where
    let mut reasoning = format!(
        "A {} is stored in this program as {origin}. ",
        class.label()
    );
    // then say what corroborates it, and just as plainly what does not
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
             value being sent rather than tested — and co-location on its own is reported as \
             identity context, not as keying.",
            record.describe(),
        )),
        (None, _) => {
            // a runtime wrapper among the users does read identity, so the honest
            // sentence is about whose data this is, not that nothing reads the class
            if wrapper_user {
                reasoning.push_str(
                    "The only identity-reading code referencing it is a linked runtime's own \
                     routine, which reaches its own data this way as a matter of course — that \
                     is the runtime being present, not the program checking identity against a \
                     stored key.",
                );
            } else {
                reasoning.push_str(
                    "Nothing in this program was observed reading the machine's own value of this \
                     class, so there is no evidence the literal is used as a guardrail. It is \
                     reported because the value itself is pivotable — it names a specific machine, \
                     adapter, or account — but a literal of this shape is equally often a vendor \
                     identifier, a protocol constant, a test vector, or documentation.",
                );
            }
            if !named.is_empty() {
                reasoning.push_str(&format!(" It is referenced by `{named}`."));
            }
        }
    }
    reasoning
}

/// The enumerating function a recovered value is attributed to, if any qualifies.
///
/// Prefers a user that enumerates the value's own class, then any user that
/// enumerates identity at all. A linked runtime's own wrapper never qualifies: a
/// marker match means the function *is* the runtime routine (`os.Hostname`), so
/// its references are the runtime reaching its own tables rather than the
/// author's check, and letting it anchor a value would dress library data up as
/// co-location.
///
/// # Arguments
///
/// * `users` - The functions referencing the value, in address order
/// * `enumerators` - The functions found reading machine identity
/// * `class` - The value's identity class
fn anchor_for<'a>(
    users: &[u64],
    enumerators: &'a HashMap<u64, EnumeratingFunction>,
    class: IdentityClass,
) -> Option<&'a EnumeratingFunction> {
    // the users that enumerate identity and are not a runtime's own routine
    let qualified: Vec<&EnumeratingFunction> = users
        .iter()
        .filter_map(|entry| enumerators.get(entry))
        .filter(|record| !record.runtime_wrapper)
        .collect();
    // prefer the user reading this very class, then any identity reader at all
    qualified
        .iter()
        .find(|record| record.classes.contains(&class))
        .copied()
        .or_else(|| qualified.first().copied())
}

/// Score an identity hit from its attribution and comparison evidence.
///
/// The module's severity ladder, kept in one auditable step: the
/// environmental-keying claim — **T1480.001** at [`medium`] with
/// [`EvidenceKind::ControlFlowGated`] — is reserved for a value an enumerating
/// function visibly compares in a branch condition, which is the only shape the
/// validator can then confirm. Everything short of that (co-location with an
/// enumerator, a comparison that guards nothing, a bare literal) is reported as
/// **T1082** identity context at [`low`], because co-location alone does not
/// evidence keying: victim-id generators, recon writers, and self-checks all
/// read identity to *use* it rather than to test it.
///
/// # Arguments
///
/// * `matched` - Whether an enumerating function (not a runtime wrapper) uses the value
/// * `comparison` - What [`comparison_in`] observed in that function, when one uses it
fn score_identity(
    matched: bool,
    comparison: Option<bool>,
) -> (AttackTechnique, (Confidence, i64), EvidenceKind) {
    match (matched, comparison) {
        // a comparison feeding a branch is the guardrail itself; the validator
        // re-checks the control flow, which is what `ControlFlowGated` requests
        (true, Some(true)) => (T1480_001, medium(), EvidenceKind::ControlFlowGated),
        // anything less is context for the Discovery picture, never a keying claim
        _ => (T1082, low(), EvidenceKind::ContextOnly),
    }
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

    /// The byte scan recovers structured GUIDs and never carves MAC addresses.
    #[test]
    fn scan_window_recovers_guids_and_never_carves_macs() {
        // a version-4, RFC 4122-variant GUID is recovered wherever it sits; the
        // filler's version nibble (0xc) keeps it from forming a second candidate
        let mut window = vec![0xcc; 8];
        window.extend_from_slice(&[
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x48, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x00,
        ]);
        window.extend_from_slice(&[0xcc; 8]);
        let found = scan_window(&window);
        assert_eq!(
            found,
            vec![(8, "44332211-6655-4877-99aa-bbccddeeff00".to_string())]
        );
        // no six-byte window is ever claimed as a MAC address, whatever it renders
        // as. The MSVC /GS cookie constant, whose bytes read as `32:a2:df:2d:99:2b`...
        let mut cookie = 0x0000_2B99_2DDF_A232u64.to_le_bytes().to_vec();
        cookie.extend_from_slice(&[0xcc; 16]);
        assert_eq!(scan_window(&cookie), vec![]);
        // ...the repeating banner filler that reads as `a2:c2:a2:c2:a2:c2`...
        assert_eq!(scan_window(&[0xa2, 0xc2].repeat(16)), vec![]);
        // ...the CRT case-map stride that reads as `10:10:10:10:10:00`...
        let mut case_map = vec![0x10; 16];
        case_map[5] = 0x00;
        assert_eq!(scan_window(&case_map), vec![]);
        // ...and even a perfectly plausible stored adapter address: six raw bytes
        // carry no verifiable structure, so no byte scan ever claims one
        let mut mac = vec![0x08, 0x00, 0x27, 0xaa, 0xbb, 0xcc];
        mac.extend_from_slice(&[0x00; 18]);
        assert_eq!(scan_window(&mac), vec![]);
    }

    /// The keying claim is reserved for a comparison observed in a branch.
    #[test]
    fn score_identity_reserves_the_keying_claim_for_a_branch_comparison() {
        // a branch comparison in an enumerating user: the guardrail tier
        let (technique, severity, kind) = score_identity(true, Some(true));
        assert_eq!(technique.id, "T1480.001");
        assert!(matches!(severity.0, Confidence::Unsure));
        assert_eq!(severity.1, medium().1);
        assert_eq!(kind, EvidenceKind::ControlFlowGated);
        // co-location with no visible comparison: Discovery context, never keying
        let (technique, severity, kind) = score_identity(true, None);
        assert_eq!(technique.id, "T1082");
        assert!(matches!(severity.0, Confidence::Untrusted));
        assert_eq!(severity.1, low().1);
        assert_eq!(kind, EvidenceKind::ContextOnly);
        // a comparison that guards nothing scores the same as none
        let (technique, _, kind) = score_identity(true, Some(false));
        assert_eq!(technique.id, "T1082");
        assert_eq!(kind, EvidenceKind::ContextOnly);
        // a bare literal with no enumerating user at all
        let (technique, severity, kind) = score_identity(false, None);
        assert_eq!(technique.id, "T1082");
        assert!(matches!(severity.0, Confidence::Untrusted));
        assert_eq!(kind, EvidenceKind::ContextOnly);
    }

    /// Identity reads inside recognized library code are skipped; the sample's own seed.
    #[test]
    fn enumeration_hits_skip_recognized_library_functions() {
        // a glibc resolver internal and the sample's own reader, both enumerating
        let enumerators: HashMap<u64, EnumeratingFunction> = [
            (
                0x1000,
                enumerator(0x1000, IdentityClass::MachineName, "gethostname"),
            ),
            (
                0x2000,
                enumerator(0x2000, IdentityClass::MachineName, "GetComputerName"),
            ),
        ]
        .into_iter()
        .collect();
        // the resolver names 0x1000 as glibc internals; 0x2000 is the sample's own
        let hits = enumeration_hits(
            |entry| (entry == 0x1000).then(|| "__nss_hostname_digits_dots".to_string()),
            &enumerators,
        );
        // only the sample's own reader survives the library skip
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].function, Some(0x2000));
    }

    /// Attribution prefers the class's own reader and never a runtime wrapper.
    #[test]
    fn anchor_for_never_selects_a_runtime_wrapper() {
        // the runtime's own hostname routine, plus two user-code enumerators
        let mut wrapper = enumerator(0x1000, IdentityClass::MachineName, "Go os.Hostname");
        wrapper.runtime_wrapper = true;
        let enumerators: HashMap<u64, EnumeratingFunction> = [
            (0x1000, wrapper),
            (
                0x2000,
                enumerator(0x2000, IdentityClass::AccountName, "GetUserName"),
            ),
            (
                0x3000,
                enumerator(0x3000, IdentityClass::MachineName, "GetComputerName"),
            ),
        ]
        .into_iter()
        .collect();
        // a wrapper-only user set anchors nothing: library data is not co-location
        assert!(anchor_for(&[0x1000], &enumerators, IdentityClass::MachineName).is_none());
        // with user code among the users, the class's own reader wins
        let anchored = anchor_for(
            &[0x1000, 0x2000, 0x3000],
            &enumerators,
            IdentityClass::MachineName,
        )
        .expect("a user-code anchor");
        assert_eq!(anchored.entry, 0x3000);
        // and any identity reader beats no anchor when the class has no reader
        let fallback = anchor_for(&[0x1000, 0x2000], &enumerators, IdentityClass::MachineName)
            .expect("the fallback anchor");
        assert_eq!(fallback.entry, 0x2000);
    }

    /// The decoy, encoded, and argument-only literals the comparison gate drops.
    #[test]
    fn name_gate_drops_literals_without_a_visible_comparison() {
        // a crypter decoy: written once to a never-read global, never compared
        let decoy = "  *(char **)0x466660 = \"MrwxeppHexi\";\n";
        assert_eq!(comparison_in(decoy, "MrwxeppHexi"), None);
        // a brute-force credential: passed as an argument, never compared
        let argument = "  uVar1 = LogonUserA(\"Administrator\",local_20,pcVar3,2,0,&local_8);\n";
        assert_eq!(comparison_in(argument, "Administrator"), None);
        // the true-positive shape keeps its evidence: a name checked in a branch
        let guarded = "  if (strcmp(local_buf, \"WIN-TARGET-01\") == 0) goto run_payload;\n";
        assert_eq!(comparison_in(guarded, "WIN-TARGET-01"), Some(true));
    }

    /// Registry value names and resolver variables are enumeration inputs, not keys.
    #[test]
    fn enumeration_inputs_are_not_name_candidates() {
        // the registry value names an identity reader passes to RegQueryValueEx
        assert!(is_enumeration_input("InstallDate"));
        assert!(is_enumeration_input("RegisteredOwner"));
        assert!(is_enumeration_input("RegisteredOrganization"));
        assert!(is_enumeration_input("MachineGuid"));
        assert!(is_enumeration_input("DigitalProductId"));
        assert!(is_enumeration_input("ProductId"));
        // matching is case-insensitive, as the registry itself is
        assert!(is_enumeration_input("machineguid"));
        // the resolver environment variables glibc consults
        assert!(is_enumeration_input("RES_OPTIONS"));
        assert!(is_enumeration_input("HOSTALIASES"));
        assert!(is_enumeration_input("LOCALDOMAIN"));
        // a hostname-shaped literal is still a candidate
        assert!(!is_enumeration_input("WIN-TARGET-01"));
        // and so is an encoded blob — the comparison gate judges those instead
        assert!(!is_enumeration_input("MrwxeppHexi"));
    }

    /// The name candidates past the cap fold into one artifact-less summary.
    #[test]
    fn name_overflow_hit_summarizes_the_candidates_past_the_cap() {
        // nothing past the cap, nothing to say
        assert!(name_overflow_hit("FUN_00442cb5", 0x44_2cb5, &[]).is_none());
        let overflow = vec![
            NameCandidate {
                address: 0x1000,
                text: "HOST-A".to_string(),
            },
            NameCandidate {
                address: 0x1008,
                text: "HOST-B".to_string(),
            },
        ];
        let hit =
            name_overflow_hit("FUN_00442cb5", 0x44_2cb5, &overflow).expect("a summary finding");
        // one low Discovery line, carrying no artifact, anchored at the function
        assert_eq!(hit.technique.id, "T1082");
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, low().1);
        assert_eq!(hit.kind, EvidenceKind::ContextOnly);
        assert!(hit.artifact.is_none());
        assert_eq!(hit.function, Some(0x44_2cb5));
        // it counts and names what was folded
        assert!(hit.content.contains("2 further"));
        assert!(hit.reasoning.contains("`HOST-A`"));
        assert!(hit.reasoning.contains("`HOST-B`"));
    }

    /// One string's GUIDs come back as one group, platform ids already dropped.
    #[test]
    fn split_string_matches_groups_guids_and_drops_platform_ids() {
        // a config string holding two unique GUIDs, one OLE automation id, and a MAC
        let matches = find_host_identifiers(
            "cfg 916d809c-5949-4ca3-b1d4-7e2f0a68c315 then 00020400-0000-0000-c000-000000000046 \
             and ac634028-9bf2-4876-8a52-6b43e4b1f2d7 via 00:1a:2b:3c:4d:5e",
        );
        let (guids, others) = split_string_matches(matches);
        // the two unique GUIDs form one group; the platform id is dropped
        assert_eq!(guids.len(), 2);
        assert!(
            guids
                .iter()
                .any(|found| found.value.starts_with("916d809c"))
        );
        assert!(
            guids
                .iter()
                .any(|found| found.value.starts_with("ac634028"))
        );
        // the MAC literal passes through for its own per-value finding
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].kind, HostIdKind::MacAddress);
    }

    /// The unattributed GUIDs past the cap fold into one bounded summary.
    #[test]
    fn guid_overflow_hit_summarizes_the_dropped_values() {
        // nothing dropped, nothing to say
        assert!(guid_overflow_hit(&[]).is_none());
        // fourteen dropped values: twelve are spelled out, the rest are counted
        let overflow: Vec<(String, u64)> = (0u64..14)
            .map(|index| {
                (
                    format!("0000000{index:x}-aaaa-4bbb-8ccc-ddddeeee0000"),
                    0x40_0000 + index,
                )
            })
            .collect();
        let hit = guid_overflow_hit(&overflow).expect("a summary finding");
        // one low Discovery line anchored at the first dropped value, no artifact
        assert_eq!(hit.technique.id, "T1082");
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert!(hit.artifact.is_none());
        assert_eq!(hit.address, 0x40_0000);
        assert!(hit.content.contains("14 further GUID literals"));
        // the listing is bounded so the summary stays one line
        assert!(
            hit.reasoning
                .contains("`00000000-aaaa-4bbb-8ccc-ddddeeee0000`")
        );
        assert!(hit.reasoning.contains("and 2 more"));
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

    /// The cap is per class, caller-set, and shared across the counting passes.
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
            assert!(admits(
                &mut counts,
                IdentityClass::MacAddress,
                MAX_PER_CLASS,
                source
            ));
        }
        // saturated, and it stays saturated rather than resetting for a later pass
        assert!(!admits(
            &mut counts,
            IdentityClass::MacAddress,
            MAX_PER_CLASS,
            "literal pool"
        ));
        // a different class has its own allowance, under its own cap: the tighter
        // unattributed GUID allowance refuses a seventh value
        for _ in 0..MAX_UNATTRIBUTED_GUIDS {
            assert!(admits(
                &mut counts,
                IdentityClass::Guid,
                MAX_UNATTRIBUTED_GUIDS,
                "string table"
            ));
        }
        assert!(!admits(
            &mut counts,
            IdentityClass::Guid,
            MAX_UNATTRIBUTED_GUIDS,
            "literal pool"
        ));
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
            runtime_wrapper: false,
        }
    }

    /// A block holding one stored GUID, with every reference reaching it.
    ///
    /// The GUID leads the block so nothing precedes it to be sliced with, and the
    /// filler behind it is one repeated byte whose version nibble (0xa) is not a
    /// defined UUID version — so the block holds exactly one candidate and a test
    /// over it is measuring the bookkeeping rather than the scan.
    fn stored_value_section() -> Section {
        let mut bytes = vec![
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x48, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
            0xff, 0x00,
        ];
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
                enumerator(0x1000, IdentityClass::Guid, "GetSystemFirmwareTable"),
            ),
            (
                0x2000,
                enumerator(0x2000, IdentityClass::Guid, "Rust machine-uid"),
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
        let ((at, value), (section, users)) = found.into_iter().next().unwrap();
        assert_eq!(at, 0x40_1000);
        assert_eq!(value, "44332211-6655-4877-99aa-bbccddeeff00");
        assert_eq!(section.name, ".data");
        // the second function is more attribution for the one value, not a second
        // finding, and the repeated reference is not a third
        assert_eq!(users, vec![0x2000, 0x1000]);
    }

    /// Only an observed GUID read justifies the byte scan, and nothing else joins it.
    #[test]
    fn collect_stored_values_scopes_the_scan_to_guid_readers() {
        let sections = vec![stored_value_section()];
        // an adapter-address reader does not scope a byte scan at all: six raw MAC
        // bytes carry no structure, so no byte-level search exists for that class
        let enumerators: HashMap<u64, EnumeratingFunction> = [(
            0x3000,
            enumerator(0x3000, IdentityClass::MacAddress, "GetAdaptersInfo"),
        )]
        .into_iter()
        .collect();
        let index = [(0x40_1000, 0x3000)];
        assert!(collect_stored_values(&sections, &index, &enumerators).is_empty());
        // and it stays out of the attribution when another function does read one
        let mut enumerators = enumerators;
        enumerators.insert(
            0x1000,
            enumerator(0x1000, IdentityClass::Guid, "GetSystemFirmwareTable"),
        );
        let index = [(0x40_1000, 0x3000), (0x40_1000, 0x1000)];
        let found = collect_stored_values(&sections, &index, &enumerators);
        assert_eq!(found.len(), 1);
        assert_eq!(found.into_values().next().unwrap().1, vec![0x1000]);
    }

    /// A linked runtime's own wrapper never anchors the stored-value scan.
    #[test]
    fn collect_stored_values_skips_runtime_wrappers() {
        let sections = vec![stored_value_section()];
        // the runtime's own GUID routine referencing the buffer scopes nothing
        let mut wrapper = enumerator(0x1000, IdentityClass::Guid, "Rust machine-uid");
        wrapper.runtime_wrapper = true;
        let enumerators: HashMap<u64, EnumeratingFunction> =
            [(0x1000, wrapper)].into_iter().collect();
        let index = [(0x40_1000, 0x1000)];
        assert!(collect_stored_values(&sections, &index, &enumerators).is_empty());
        // the same reference from ordinary user code is scanned
        let enumerators: HashMap<u64, EnumeratingFunction> = [(
            0x1000,
            enumerator(0x1000, IdentityClass::Guid, "GetSystemFirmwareTable"),
        )]
        .into_iter()
        .collect();
        assert_eq!(
            collect_stored_values(&sections, &index, &enumerators).len(),
            1
        );
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
        // is bytes of two different fields, not one stored value
        assert!(straddles_pointer(&map, &data, 0x1_4004_487e, GUID_BYTE_LEN));
        // and so is one spanning the end of a mangled name and the pointer that
        // opens the next descriptor
        assert!(straddles_pointer(&map, &data, 0x1_4004_48ac, GUID_BYTE_LEN));
        // a stored field in its own slot shares no byte with a pointer
        assert!(!straddles_pointer(
            &map,
            &data,
            0x1_4004_48c0,
            GUID_BYTE_LEN
        ));
        // and a word decoding to an address outside every loaded block is a number,
        // so the descriptor's integer field claims nothing
        assert!(!straddles_pointer(&map, &data, 0x1_4004_4878, 6));
    }
}
