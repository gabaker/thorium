//! Per-class validity gates for pivot-class artifacts.
//!
//! A pivot-class artifact (a config, endpoint, key, certificate, or host
//! identifier) earns privileges elsewhere in the engine: it vetoes
//! runtime-noise classification
//! ([`runtime_classification`](super::detections::runtime_classification)) and
//! bucket suppression (the seeder's `partition_suppressed`), because a real
//! recovery must stay loud wherever it sits. The fleet telemetry showed those
//! privileges shielding fabricated values too — carved degenerate MACs, junk
//! "recovered" plaintext — which made artifact-carrying leads the dispute
//! epicenter. This module decides, per class, whether a recovered value
//! actually deserves the privileges. An unvalidated artifact loses only the
//! veto, never the hit: demote-not-drop.
//!
//! Consumers: `runtime_classification`'s artifact exemption is wired through
//! [`artifact_is_validated`], and the seeder (`crate::thorpal::seed`) leans
//! on the same gate in three places — the bucket-suppression veto
//! (`has_protected` inside `partition_suppressed`), the admission-budget
//! bypass and ranking (`bypasses_budget` / `apply_admission_budget`), and the
//! evasion-question verified-evidence gate (`evasion_question`).
//!
//! Classes with no gate yet pass (fail-open), so adding an
//! [`ArtifactClass`] variant never silently strips privileges.

use crate::thorpal::detect::detections::{ArtifactClass, DetectionHit};
use crate::thorpal::detect::indicators::{
    is_degenerate_mac, parse_endpoint, parse_url, plausible_endpoint_host,
};

/// The ASCII punctuation the plaintext printability ratio accepts.
///
/// Deliberately a modest set: decode junk leans on repeated `~`/`|`/`^`-class
/// bytes, so counting those as printable would let tilde soup pass the ratio.
const COMMON_PUNCT: &[u8] = b".,:;!?'\"()-_/\\@=+#&*%";

/// Whether a hit's artifact deserves the pivot-class privileges.
///
/// Applies the per-class validity gate to the artifact the hit carries:
/// host identifiers (GUIDs pass; MACs must look like assigned addresses),
/// recovered-plaintext/key classes (a wordiness/printability gate),
/// network endpoints (host syntax), and certificate/asymmetric-key material
/// (PEM/DER shape). Classes with no gate yet pass, and a hit carrying no
/// artifact has no privileges to earn.
///
/// # Arguments
///
/// * `hit` - The hit whose artifact to judge
pub fn artifact_is_validated(hit: &DetectionHit) -> bool {
    // a hit with no artifact has no artifact privileges to earn
    let Some(artifact) = &hit.artifact else {
        return false;
    };
    // an empty value identifies nothing, whatever its class claims
    let value = artifact.value.trim();
    if value.is_empty() {
        return false;
    }
    // judge the value by its class's own gate
    match artifact.class {
        ArtifactClass::HostIdentifier => host_identifier_is_valid(value),
        ArtifactClass::SymmetricKey => is_plausible_text(value) || is_key_material_shape(value),
        ArtifactClass::ConfigRecord => is_plausible_text(value),
        ArtifactClass::NetworkEndpoint => endpoint_is_valid(value),
        ArtifactClass::Certificate
        | ArtifactClass::AsymmetricPrivateKey
        | ArtifactClass::AsymmetricPublicKey => is_pem_or_der_shape(value),
        // classes with no gate yet keep their privileges (fail-open)
        _ => true,
    }
}

/// Whether a host-identifier value deserves its class's privileges.
///
/// GUID-shaped values always pass (the fleet's confirmed `HostIdentifier`
/// artifacts are untrusted GUID literals the veto saves today); MAC-shaped
/// values must pass the shared degeneracy tests plus a local pattern test
/// (the binary-carving path manufactures MAC shapes out of byte tables);
/// the remaining host-id shapes (volume serials, machine names) have no gate
/// yet and pass.
///
/// # Arguments
///
/// * `value` - The recovered identifier rendering
fn host_identifier_is_valid(value: &str) -> bool {
    // a GUID keeps its privileges: real author-chosen GUIDs are the confirmed class
    if is_guid_shaped(value) {
        return true;
    }
    // a MAC must look like an assigned address, not a carved byte pattern
    if let Some(octets) = parse_mac_octets(value) {
        return mac_is_plausible(octets);
    }
    // no gate for the remaining host-id shapes yet: fail open
    true
}

/// Whether a value is the canonical `8-4-4-4-12` hex GUID rendering.
///
/// # Arguments
///
/// * `value` - The candidate rendering, braces optional, any case
fn is_guid_shaped(value: &str) -> bool {
    /// The hex-digit lengths of the canonical GUID groups.
    const GUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    // strip optional braces, then demand exactly the five hex groups
    let trimmed = value.trim().trim_start_matches('{').trim_end_matches('}');
    let groups: Vec<&str> = trimmed.split('-').collect();
    groups.len() == GUID_GROUPS.len()
        && groups.iter().zip(GUID_GROUPS).all(|(group, want)| {
            group.len() == want && group.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

/// Parse the six octets out of a colon/dash-form MAC rendering.
///
/// Returns `None` when the value is not exactly six two-hex-digit groups —
/// such values are not MAC-shaped and are not judged by the MAC tests. This
/// reimplements the small text parser private to `super::indicators`
/// (`is_degenerate_mac_text`), which another lane owns; the degeneracy tests
/// themselves are reused from the public [`is_degenerate_mac`].
///
/// # Arguments
///
/// * `value` - The candidate rendering
fn parse_mac_octets(value: &str) -> Option<[u8; 6]> {
    // six separator-delimited groups of exactly two hex digits
    let groups: Vec<&str> = value.trim().split([':', '-']).collect();
    if groups.len() != 6 || groups.iter().any(|group| group.len() != 2) {
        return None;
    }
    // parse each group; any non-hex group means the shape is not a MAC
    let mut octets = [0u8; 6];
    for (slot, group) in octets.iter_mut().zip(groups) {
        *slot = u8::from_str_radix(group, 16).ok()?;
    }
    Some(octets)
}

/// Whether six MAC octets look like an assigned adapter address.
///
/// Runs the shared [`is_degenerate_mac`] tests (sentinels, fills, monotone
/// runs, the group bit, sparse small-number tables), then one local pattern
/// test the shared gate lacks: an address built from at most two distinct
/// byte values (`a2:c2:a2:c2:a2:c2`, `00:00:ff:ff:ff:ff`) is a repeating
/// fill/carve pattern, never an assigned OUI plus device id.
///
/// # Arguments
///
/// * `octets` - The six octets, in wire order
fn mac_is_plausible(octets: [u8; 6]) -> bool {
    // the shared degeneracy tests reject sentinels and table carves
    if is_degenerate_mac(&octets) {
        return false;
    }
    // count the distinct byte values in the rendering
    let mut distinct: Vec<u8> = octets.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    // two or fewer distinct bytes is a repeating pattern, not an address
    distinct.len() > 2
}

/// Whether recovered text is plausibly real plaintext rather than decode junk.
///
/// The compact local gate for recovered-plaintext privileges. Heuristic: at
/// least 60% of the characters are ASCII letters, digits, spaces, or common
/// punctuation, AND the text either contains a vowel-bearing word of four or
/// more letters or carries a known format shape (URL, filesystem path, or
/// email). The decode engine owns the heavier wordiness/table-run analyses;
/// this gate only decides pivot privileges, so it errs toward keeping.
///
/// # Arguments
///
/// * `value` - The recovered text
fn is_plausible_text(value: &str) -> bool {
    // measure the printable share of the text
    let total = value.chars().count();
    if total == 0 {
        return false;
    }
    let printable = value
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric()
                || *c == ' '
                || (c.is_ascii() && COMMON_PUNCT.contains(&(*c as u8)))
        })
        .count();
    // the 60% floor, computed without floating point
    if printable * 10 < total * 6 {
        return false;
    }
    // real text carries pronounceable words (or a recognizable structure);
    // junk byte runs carry neither
    has_vowel_word(value) || known_format_shape(value)
}

/// Whether the text contains a word of four or more letters bearing a vowel.
///
/// # Arguments
///
/// * `value` - The text to scan
fn has_vowel_word(value: &str) -> bool {
    // split into alphabetic runs and look for one pronounceable word
    value.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
        word.len() >= 4
            && word
                .chars()
                .any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'))
    })
}

/// Whether the text carries a recognizable structured shape (URL, path, email).
///
/// The escape hatch for recovered values that are structured rather than
/// prose, so a wordless-but-real recovery (a bare URL, a dropped-file path)
/// still passes the plaintext gate.
///
/// # Arguments
///
/// * `value` - The text to scan
fn known_format_shape(value: &str) -> bool {
    // a parseable URL anywhere in the text is a recognizable recovery
    if parse_url(value).is_some() {
        return true;
    }
    // an email-shaped token: non-empty local part at a plausible dotted domain
    let email = value.split_whitespace().any(|token| {
        token
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && plausible_endpoint_host(domain))
    });
    if email {
        return true;
    }
    // a filesystem path: rooted unix, UNC, or drive-letter form
    let trimmed = value.trim();
    let unix = trimmed.starts_with('/') && trimmed[1..].contains('/');
    let unc = trimmed.starts_with("\\\\");
    let drive = matches!(trimmed.as_bytes().first(), Some(b) if b.is_ascii_alphabetic())
        && trimmed.as_bytes().get(1) == Some(&b':')
        && matches!(trimmed.as_bytes().get(2), Some(b'\\' | b'/'));
    unix || unc || drive
}

/// Whether a value is rendered key material: an unbroken hex or base64 run.
///
/// A real symmetric key is often stored or rendered as hex/base64 rather than
/// prose, so the plaintext gate alone would wrongly strip such recoveries of
/// their privileges; this keeps them. The thresholds are deliberately low
/// (eight hex digits / sixteen base64 characters) and the only extra demand
/// is a little byte diversity, because this gate errs toward keeping — the
/// decode lane owns the real key validation.
///
/// # Arguments
///
/// * `value` - The recovered key rendering
fn is_key_material_shape(value: &str) -> bool {
    let trimmed = value.trim();
    // an even-length hex run of at least eight digits (four key bytes)
    let hex = trimmed.len() >= 8
        && trimmed.len() % 2 == 0
        && trimmed.bytes().all(|b| b.is_ascii_hexdigit());
    // or a base64-charset run long enough to be encoded key bytes
    let base64 = trimmed.len() >= 16
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    // either shape needs some diversity: a repeated-byte run is a fill pattern
    let mut bytes: Vec<u8> = trimmed.bytes().collect();
    bytes.sort_unstable();
    bytes.dedup();
    (hex || base64) && bytes.len() >= 4
}

/// Whether a network-endpoint value is syntactically usable as an endpoint.
///
/// Accepts a parseable URL, a `host:port` with a plausible host and sane
/// port, or a bare dotted host / IPv4 literal (a path/query suffix is
/// tolerated); rejects whitespace, control characters, and punctuation soup
/// that happens to carry a colon.
///
/// # Arguments
///
/// * `value` - The recovered endpoint rendering
fn endpoint_is_valid(value: &str) -> bool {
    // an endpoint is one token of printable text
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    // the URL form must parse whole (scheme, host, optional port)
    if trimmed.contains("://") {
        return parse_url(trimmed).is_some();
    }
    // otherwise judge the authority before any path/query suffix
    let head = trimmed.split(['/', '?', '#']).next().unwrap_or_default();
    // host:port with a sane port, or a bare dotted host / IP literal
    parse_endpoint(head).is_some() || plausible_endpoint_host(head)
}

/// Whether a value is plausibly PEM/DER-ish key or certificate material.
///
/// Accepts a PEM block opening with `-----BEGIN` and a sane label (uppercase
/// letters, digits, and spaces up to the closing dashes), or a hex/base64 run
/// long enough to be DER bytes. Garbage labels and tilde-soup bodies fail.
///
/// # Arguments
///
/// * `value` - The recovered material
fn is_pem_or_der_shape(value: &str) -> bool {
    let trimmed = value.trim_start();
    // the PEM form: the opening boundary plus a label of uppercase words
    if let Some(rest) = trimmed.strip_prefix("-----BEGIN") {
        // the label runs to the closing dashes of the opening boundary
        let Some((label, _)) = rest.split_once("-----") else {
            return false;
        };
        let label = label.trim();
        // an empty label or non-label characters mean this is not a boundary
        return !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ' ');
    }
    // the raw form: a hex or base64 run long enough to be DER material
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    let hex = compact.len() >= 32 && compact.bytes().all(|b| b.is_ascii_hexdigit());
    let base64 = compact.len() >= 32
        && compact
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    hex || base64
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::borrow::Cow;

    use crate::thorpal::detect::detections::{AttackTactic, AttackTechnique, HitArtifact, medium};

    /// A minimal technique for building test hits.
    const TECH: AttackTechnique = AttackTechnique {
        id: Cow::Borrowed("T1480"),
        name: Cow::Borrowed("Execution Guardrails"),
        tactic: AttackTactic::DefenseEvasion,
    };

    /// Build a hit carrying one artifact of the given class and value.
    ///
    /// # Arguments
    ///
    /// * `class` - The artifact class under test
    /// * `value` - The recovered value under test
    fn hit_with(class: ArtifactClass, value: &str) -> DetectionHit {
        DetectionHit::new(
            TECH,
            0x401000,
            Some(0x401000),
            medium(),
            "content",
            "reasoning",
        )
        .artifact(HitArtifact::new(class, value))
    }

    /// Carved pattern MACs fail the gate while GUIDs and real MACs keep the veto.
    #[test]
    fn host_identifiers_gate_macs_and_keep_guids() {
        // the telemetry's carved byte patterns are not addresses
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::HostIdentifier,
            "a2:c2:a2:c2:a2:c2"
        )));
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::HostIdentifier,
            "00:00:ff:ff:ff:ff"
        )));
        // a plausible assigned address keeps its privileges
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::HostIdentifier,
            "60:45:cb:9f:8e:21"
        )));
        // any syntactically valid GUID stays protected (the confirmed class)
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::HostIdentifier,
            "AC634028-9BF2-4EC5-9D3B-2A5C8D1E7F10"
        )));
        // non-MAC, non-GUID host-id shapes have no gate yet: fail open
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::HostIdentifier,
            "WIN-ANALYST01"
        )));
    }

    /// Junk decode output fails the plaintext gate while wordy recoveries pass.
    #[test]
    fn recovered_text_gate_rejects_junk_and_keeps_words() {
        // the telemetry's junk "recovered" plaintext earns no privileges
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::ConfigRecord,
            "~~|~||z~||z|zzx"
        )));
        // a ransom-note-like sentence is real plaintext
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::ConfigRecord,
            "All your files have been encrypted! Contact us to recover them."
        )));
        // a hex-rendered symmetric key is a plausible key rendering
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::SymmetricKey,
            "deadbeefcafef00d"
        )));
        // tilde soup is not a key either
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::SymmetricKey,
            "~~|~||z~||z|zzx"
        )));
    }

    /// Endpoint values pass on URL / host:port / dotted-host syntax only.
    #[test]
    fn endpoints_gate_on_host_syntax() {
        // a real URL (the telemetry's confirmed endpoint shape) passes
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::NetworkEndpoint,
            "https://blog.yahoo.com/beacon/check"
        )));
        // a bare ip:port endpoint passes
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::NetworkEndpoint,
            "10.2.3.4:8443"
        )));
        // prose and key:value text never name a host
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::NetworkEndpoint,
            "not a host"
        )));
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::NetworkEndpoint,
            "error:42"
        )));
    }

    /// Certificate/key material passes on PEM/DER shape and fails on garbage.
    #[test]
    fn certificates_gate_on_pem_or_der_shape() {
        // a labeled PEM boundary is plausible material
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::Certificate,
            "-----BEGIN CERTIFICATE-----\nMIIBpTCCAQ4="
        )));
        // a long hex run is plausibly DER bytes
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::AsymmetricPublicKey,
            "30820122300d06092a864886f70d01010105000382010f00"
        )));
        // a boundary with garbage where the label belongs is not PEM
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::AsymmetricPrivateKey,
            "-----BEGIN ~~~~-----"
        )));
        // an empty value identifies nothing
        assert!(!artifact_is_validated(&hit_with(
            ArtifactClass::Certificate,
            "   "
        )));
    }

    /// Ungated classes fail open and an artifact-less hit earns nothing.
    #[test]
    fn ungated_classes_fail_open() {
        // classes with no gate yet keep their privileges
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::Packer,
            "UPX0/UPX1 section pair"
        )));
        assert!(artifact_is_validated(&hit_with(
            ArtifactClass::DecodedData,
            "anything"
        )));
        // no artifact means no privileges to earn
        let bare = DetectionHit::new(TECH, 0x401000, None, medium(), "content", "reasoning");
        assert!(!artifact_is_validated(&bare));
    }
}
