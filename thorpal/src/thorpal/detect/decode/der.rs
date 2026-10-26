//! DER/ASN.1 shape recognition for key material stored without PEM armor.
//!
//! PEM armor is easy to find because it announces itself. The same key stored as
//! raw DER — which is what a program that never round-trips through text does —
//! is invisible to a boundary scan, so a public key, private key, or certificate
//! compiled in as a byte array is missed entirely by the armor pass.
//!
//! # Why this is a *validator*, not a pattern match
//!
//! The naive approach is to look for the `0x30 0x82` bytes a certificate usually
//! starts with. That is two bytes; it occurs constantly in ordinary binary data,
//! and every match would be a false positive. So nothing here is claimed on a
//! header byte. A candidate is accepted only when all three of these hold:
//!
//! 1. It parses as a **complete** DER `SEQUENCE` — every nested tag-length-value
//!    is well formed, uses DER's minimal length encoding, and no length runs past
//!    the buffer.
//! 2. Its children **exactly tile** their parent's declared content, with no
//!    leftover bytes anywhere in the tree. A run of arbitrary bytes essentially
//!    never tiles.
//! 3. The resulting tree matches one of the **known key structures** in
//!    [`DerKind`]. A structurally valid but unrecognized `SEQUENCE` is rejected,
//!    because "some ASN.1 object" is not a finding.
//!
//! Together those three make a false positive require arbitrary data to be a
//! self-consistent ASN.1 tree of exactly the right shape, which is a far stronger
//! filter than any byte signature.
//!
//! # What this deliberately does not do
//!
//! It does not decode OIDs, validate signatures, or check key parameters. Those
//! would need a real ASN.1 stack and buy nothing: the question here is only
//! "is there key material at this address, and of what class", which the shape
//! answers on its own.

use super::RecoveredKind;

/// The smallest DER object worth reporting, in bytes.
///
/// Below this, the structural filter is not doing enough work to be trusted: the
/// smallest real key material here is a SEC1 elliptic-curve private key at
/// roughly 120 bytes, and a short `SEQUENCE` of a couple of small integers is
/// something any serialization format produces by accident.
pub const MIN_DER_LEN: usize = 64;

/// The largest DER object accepted, in bytes.
///
/// Bounds a walk seeded on a length field that happens to be large. Real key
/// material and certificates are far under this; a certificate chain would be a
/// sequence of separate objects, each scanned on its own.
pub const MAX_DER_LEN: usize = 64 * 1024;

/// The deepest nesting the walker descends.
///
/// DER is self-describing, so a corrupt or hostile length field can otherwise
/// drive unbounded recursion. The structures recognized here nest four levels at
/// most.
const MAX_DEPTH: usize = 12;

/// The most length octets accepted in a long-form length.
///
/// Four covers any object up to 4 GiB, which is far past [`MAX_DER_LEN`]; a
/// longer length field means the bytes are not DER.
const MAX_LENGTH_OCTETS: usize = 4;

// --- the ASN.1 tags these structures are built from ---
/// Universal INTEGER.
const TAG_INTEGER: u8 = 0x02;
/// Universal BIT STRING.
const TAG_BIT_STRING: u8 = 0x03;
/// Universal OCTET STRING.
const TAG_OCTET_STRING: u8 = 0x04;
/// Universal OBJECT IDENTIFIER.
const TAG_OID: u8 = 0x06;
/// Universal SEQUENCE (constructed).
const TAG_SEQUENCE: u8 = 0x30;
/// Context-specific `[0]`, constructed.
const TAG_CTX_0: u8 = 0xa0;
/// Context-specific `[1]`, constructed.
const TAG_CTX_1: u8 = 0xa1;

/// The smallest RSA modulus accepted, in bytes.
///
/// 512 bits. Anything smaller has not been a usable RSA key for decades, and the
/// length check is what stops a `SEQUENCE` of two small integers — an extremely
/// common shape — from being read as an RSA public key.
const MIN_RSA_MODULUS: usize = 64;

/// The largest RSA public exponent accepted, in bytes.
///
/// The public exponent is essentially always 3 or 65537. Bounding it separates
/// `SEQUENCE { INTEGER huge, INTEGER tiny }` (an RSA public key) from
/// `SEQUENCE { INTEGER, INTEGER }` holding two arbitrary values.
const MAX_RSA_EXPONENT: usize = 8;

/// The private-scalar sizes accepted for an elliptic-curve private key, in bytes.
///
/// Spans P-256 through P-521 and the common Edwards curves, allowing for the
/// leading zero byte DER adds to a value whose top bit is set.
const EC_SCALAR: std::ops::RangeInclusive<usize> = 24..=68;

/// A DER structure this module recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerKind {
    /// An X.509 certificate.
    Certificate,
    /// A `SubjectPublicKeyInfo` — an algorithm identifier plus a wrapped key.
    SubjectPublicKeyInfo,
    /// A bare PKCS#1 `RSAPublicKey` (modulus and exponent).
    RsaPublicKey,
    /// A PKCS#1 `RSAPrivateKey` (the full private parameter set).
    RsaPrivateKey,
    /// A PKCS#8 `PrivateKeyInfo` — an algorithm identifier plus a wrapped key.
    Pkcs8PrivateKey,
    /// A SEC1 `ECPrivateKey`.
    EcPrivateKey,
}

impl DerKind {
    /// A human label for the structure, used in finding text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Certificate => "X.509 certificate",
            Self::SubjectPublicKeyInfo => "SubjectPublicKeyInfo public key",
            Self::RsaPublicKey => "PKCS#1 RSA public key",
            Self::RsaPrivateKey => "PKCS#1 RSA private key",
            Self::Pkcs8PrivateKey => "PKCS#8 private key",
            Self::EcPrivateKey => "SEC1 elliptic-curve private key",
        }
    }

    /// The recovered-item class this structure reports as.
    #[must_use]
    pub fn recovered_kind(self) -> RecoveredKind {
        match self {
            Self::Certificate => RecoveredKind::Certificate,
            Self::SubjectPublicKeyInfo | Self::RsaPublicKey => RecoveredKind::PublicKey,
            Self::RsaPrivateKey | Self::Pkcs8PrivateKey | Self::EcPrivateKey => {
                RecoveredKind::PrivateKey
            }
        }
    }

    /// Whether the structure carries a private key.
    ///
    /// Private material is the more serious recovery, and callers score and
    /// phrase it differently.
    #[must_use]
    pub fn is_private(self) -> bool {
        matches!(
            self,
            Self::RsaPrivateKey | Self::Pkcs8PrivateKey | Self::EcPrivateKey
        )
    }
}

/// One recognized DER object inside a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerMatch {
    /// What the object is.
    pub kind: DerKind,
    /// The byte offset the object starts at.
    pub at: usize,
    /// The object's total encoded length, header included.
    pub len: usize,
}

/// One tag-length-value header, located within a buffer.
#[derive(Debug, Clone, Copy)]
struct Tlv {
    /// The identifier octet.
    tag: u8,
    /// The offset the tag sits at.
    start: usize,
    /// The combined tag and length octet count.
    header: usize,
    /// The content length the header declares.
    len: usize,
}

impl Tlv {
    /// The offset just past this object's content.
    fn end(self) -> usize {
        self.start + self.header + self.len
    }

    /// This object's content bytes.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The buffer the object was read from
    fn content(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.start + self.header..self.end()]
    }
}

/// Recognize a DER key structure starting exactly at the front of `bytes`.
///
/// Returns `None` unless the bytes open a complete, self-consistent object of a
/// known shape. Trailing bytes past the object are allowed — the caller is
/// usually looking at a section, not at an isolated key — but the object itself
/// must be entirely present.
///
/// # Arguments
///
/// * `bytes` - The buffer to read, starting at the candidate object
#[must_use]
pub fn identify(bytes: &[u8]) -> Option<DerMatch> {
    // the outermost object is always a SEQUENCE for every structure here
    let root = read_tlv(bytes, 0)?;
    if root.tag != TAG_SEQUENCE {
        return None;
    }
    // reject the trivially small and the implausibly large before walking
    let total = root.end();
    if !(MIN_DER_LEN..=MAX_DER_LEN).contains(&total) {
        return None;
    }
    // the tree must be internally consistent before its shape means anything
    if !tiles(bytes, root, 0) {
        return None;
    }
    // and the shape must be one we can actually name
    let kind = classify(bytes, root)?;
    Some(DerMatch {
        kind,
        at: 0,
        len: total,
    })
}

/// Find every recognized DER key structure in a buffer.
///
/// Seeds only on a `SEQUENCE` tag and skips past each accepted object, so the
/// members of one certificate are never re-reported as objects of their own.
///
/// # Arguments
///
/// * `bytes` - The buffer to scan
#[must_use]
pub fn scan(bytes: &[u8]) -> Vec<DerMatch> {
    let mut found = Vec::new();
    let mut at = 0;
    while at + MIN_DER_LEN <= bytes.len() {
        // every structure here opens with a constructed SEQUENCE
        if bytes[at] != TAG_SEQUENCE {
            at += 1;
            continue;
        }
        match identify(&bytes[at..]) {
            // an accepted object: record it and resume past its end
            Some(found_here) => {
                let len = found_here.len;
                found.push(DerMatch { at, ..found_here });
                at += len;
            }
            None => at += 1,
        }
    }
    found
}

/// Read one tag-length-value header at `at`, enforcing DER's encoding rules.
///
/// # Arguments
///
/// * `bytes` - The buffer to read from
/// * `at` - The offset of the identifier octet
fn read_tlv(bytes: &[u8], at: usize) -> Option<Tlv> {
    // a header is at least an identifier octet and a length octet
    if at + 2 > bytes.len() {
        return None;
    }
    let tag = bytes[at];
    // `0x00` and `0xff` are padding, and the high-tag-number form does not appear
    // in any structure here — treating them as tags is how a walk runs away
    if tag == 0x00 || tag == 0xff || tag & 0x1f == 0x1f {
        return None;
    }
    let first = bytes[at + 1];
    let (header, len) = if first < 0x80 {
        // short form: the length is the octet itself
        (2, usize::from(first))
    } else {
        let octets = usize::from(first & 0x7f);
        // `0x80` is the indefinite form, which DER forbids; a length field longer
        // than four octets describes an object larger than this ever accepts
        if octets == 0 || octets > MAX_LENGTH_OCTETS || at + 2 + octets > bytes.len() {
            return None;
        }
        let mut len = 0usize;
        for byte in &bytes[at + 2..at + 2 + octets] {
            len = (len << 8) | usize::from(*byte);
        }
        // DER requires the minimal encoding: no leading zero octet, and the long
        // form is only legal for lengths the short form cannot express
        if bytes[at + 2] == 0 || len < 0x80 {
            return None;
        }
        (2 + octets, len)
    };
    // the declared content must actually be present
    if at.checked_add(header)?.checked_add(len)? > bytes.len() {
        return None;
    }
    Some(Tlv {
        tag,
        start: at,
        header,
        len,
    })
}

/// The immediate children of a constructed object, if they tile its content exactly.
///
/// # Arguments
///
/// * `bytes` - The buffer the parent was read from
/// * `parent` - The constructed object to descend into
fn children(bytes: &[u8], parent: Tlv) -> Option<Vec<Tlv>> {
    // only a constructed object has children
    if parent.tag & 0x20 == 0 {
        return None;
    }
    let mut kids = Vec::new();
    let mut at = parent.start + parent.header;
    let end = parent.end();
    while at < end {
        let child = read_tlv(bytes, at)?;
        // a child that runs past its parent means the lengths disagree
        if child.end() > end {
            return None;
        }
        at = child.end();
        kids.push(child);
    }
    // exact tiling: `at` lands on `end`, never inside the last child
    (at == end).then_some(kids)
}

/// Whether an object and everything under it is internally consistent.
///
/// # Arguments
///
/// * `bytes` - The buffer the object was read from
/// * `node` - The object to check
/// * `depth` - The current nesting depth
fn tiles(bytes: &[u8], node: Tlv, depth: usize) -> bool {
    // a primitive has no structure to disagree with
    if node.tag & 0x20 == 0 {
        return true;
    }
    // stop descending rather than trusting an arbitrarily deep length chain
    if depth >= MAX_DEPTH {
        return false;
    }
    // the children must tile this node, and each must itself be consistent
    match children(bytes, node) {
        Some(kids) => kids.iter().all(|kid| tiles(bytes, *kid, depth + 1)),
        None => false,
    }
}

/// Name the key structure a well-formed `SEQUENCE` represents, if it is one.
///
/// Ordered from the most constrained shape to the least, so a certificate is
/// never mistaken for the `SubjectPublicKeyInfo` nested inside it.
///
/// # Arguments
///
/// * `bytes` - The buffer the object was read from
/// * `root` - The outer `SEQUENCE`
fn classify(bytes: &[u8], root: Tlv) -> Option<DerKind> {
    let kids = children(bytes, root)?;
    // each predicate below asserts a full structure, not a prefix
    if is_certificate(bytes, &kids) {
        return Some(DerKind::Certificate);
    }
    if is_pkcs8_private(bytes, &kids) {
        return Some(DerKind::Pkcs8PrivateKey);
    }
    if is_ec_private(&kids) {
        return Some(DerKind::EcPrivateKey);
    }
    if is_rsa_private(&kids) {
        return Some(DerKind::RsaPrivateKey);
    }
    if is_spki(bytes, &kids) {
        return Some(DerKind::SubjectPublicKeyInfo);
    }
    if is_rsa_public(&kids) {
        return Some(DerKind::RsaPublicKey);
    }
    None
}

/// Whether the children form an X.509 `Certificate`.
///
/// `SEQUENCE { tbsCertificate SEQUENCE, signatureAlgorithm AlgorithmIdentifier,
/// signatureValue BIT STRING }`.
///
/// # Arguments
///
/// * `bytes` - The buffer the object was read from
/// * `kids` - The outer sequence's children
fn is_certificate(bytes: &[u8], kids: &[Tlv]) -> bool {
    // exactly the three top-level fields, in order
    if kids.len() != 3 {
        return false;
    }
    if kids[0].tag != TAG_SEQUENCE || kids[2].tag != TAG_BIT_STRING {
        return false;
    }
    // the signature algorithm is an AlgorithmIdentifier, so it opens with an OID
    if !is_algorithm_identifier(bytes, kids[1]) {
        return false;
    }
    // and the to-be-signed body carries fields of its own
    children(bytes, kids[0]).is_some_and(|tbs| tbs.len() >= 3)
}

/// Whether the children form a `SubjectPublicKeyInfo`.
///
/// `SEQUENCE { algorithm AlgorithmIdentifier, subjectPublicKey BIT STRING }`.
///
/// # Arguments
///
/// * `bytes` - The buffer the object was read from
/// * `kids` - The outer sequence's children
fn is_spki(bytes: &[u8], kids: &[Tlv]) -> bool {
    // exactly an algorithm identifier followed by the wrapped key
    if kids.len() != 2 || kids[1].tag != TAG_BIT_STRING {
        return false;
    }
    if !is_algorithm_identifier(bytes, kids[0]) {
        return false;
    }
    // the BIT STRING's first content octet is its unused-bit count, which is
    // zero for a whole number of key bytes
    let key = kids[1].content(bytes);
    key.len() > 1 && key[0] == 0
}

/// Whether the children form a PKCS#8 `PrivateKeyInfo`.
///
/// `SEQUENCE { version INTEGER, privateKeyAlgorithm AlgorithmIdentifier,
/// privateKey OCTET STRING, … }`.
///
/// # Arguments
///
/// * `bytes` - The buffer the object was read from
/// * `kids` - The outer sequence's children
fn is_pkcs8_private(bytes: &[u8], kids: &[Tlv]) -> bool {
    // the three required fields, optionally followed by attributes
    if kids.len() < 3 || kids[0].tag != TAG_INTEGER || kids[2].tag != TAG_OCTET_STRING {
        return false;
    }
    // the version is a small integer (0 for v1, 1 for v2)
    if kids[0].len > 1 {
        return false;
    }
    if !is_algorithm_identifier(bytes, kids[1]) {
        return false;
    }
    // the wrapped key is substantial — an OCTET STRING of a handful of bytes is
    // not a private key, and this is what keeps the shape from matching generic
    // `SEQUENCE { INTEGER, SEQUENCE, OCTET STRING }` serialization
    kids[2].len >= 32
}

/// Whether the children form a PKCS#1 `RSAPrivateKey`.
///
/// `SEQUENCE { version, n, e, d, p, q, dP, dQ, qInv }` — nine INTEGERs.
///
/// # Arguments
///
/// * `kids` - The outer sequence's children
fn is_rsa_private(kids: &[Tlv]) -> bool {
    // the full private parameter set, all integers
    if kids.len() < 9 || !kids.iter().take(9).all(|kid| kid.tag == TAG_INTEGER) {
        return false;
    }
    // version zero for a two-prime key
    if kids[0].len > 1 {
        return false;
    }
    // a real modulus, and a public exponent that looks like one
    kids[1].len >= MIN_RSA_MODULUS && kids[2].len <= MAX_RSA_EXPONENT
}

/// Whether the children form a bare PKCS#1 `RSAPublicKey`.
///
/// `SEQUENCE { modulus INTEGER, publicExponent INTEGER }`.
///
/// # Arguments
///
/// * `kids` - The outer sequence's children
fn is_rsa_public(kids: &[Tlv]) -> bool {
    // two integers: a large modulus and a small exponent. Both size bounds are
    // load-bearing — `SEQUENCE { INTEGER, INTEGER }` on its own is far too common
    kids.len() == 2
        && kids.iter().all(|kid| kid.tag == TAG_INTEGER)
        && kids[0].len >= MIN_RSA_MODULUS
        && kids[1].len <= MAX_RSA_EXPONENT
}

/// Whether the children form a SEC1 `ECPrivateKey`.
///
/// `SEQUENCE { version INTEGER 1, privateKey OCTET STRING, [0] parameters,
/// [1] publicKey }`.
///
/// # Arguments
///
/// * `kids` - The outer sequence's children
fn is_ec_private(kids: &[Tlv]) -> bool {
    // version and the private scalar are mandatory
    if kids.len() < 2 || kids[0].tag != TAG_INTEGER || kids[1].tag != TAG_OCTET_STRING {
        return false;
    }
    // the scalar's size is the curve's, which is what distinguishes this from an
    // arbitrary `SEQUENCE { INTEGER, OCTET STRING }`
    if !EC_SCALAR.contains(&kids[1].len) {
        return false;
    }
    // at least one of the optional context-tagged fields must be present, and
    // nothing else may follow — without this the shape is too loose to claim
    let tagged = kids[2..]
        .iter()
        .all(|kid| kid.tag == TAG_CTX_0 || kid.tag == TAG_CTX_1);
    kids.len() > 2 && tagged
}

/// Whether an object is an `AlgorithmIdentifier` (a `SEQUENCE` opening with an OID).
///
/// # Arguments
///
/// * `bytes` - The buffer the object was read from
/// * `node` - The candidate object
fn is_algorithm_identifier(bytes: &[u8], node: Tlv) -> bool {
    // a SEQUENCE whose first element names the algorithm
    if node.tag != TAG_SEQUENCE {
        return false;
    }
    children(bytes, node).is_some_and(|kids| kids.first().is_some_and(|kid| kid.tag == TAG_OID))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a DER tag-length-value with the correct short/long length form.
    ///
    /// # Arguments
    ///
    /// * `tag` - The identifier octet
    /// * `content` - The object's content bytes
    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = content.len();
        // pick the minimal length encoding DER requires
        if len < 0x80 {
            out.push(len as u8);
        } else if len <= 0xff {
            out.extend_from_slice(&[0x81, len as u8]);
        } else {
            out.extend_from_slice(&[0x82, (len >> 8) as u8, (len & 0xff) as u8]);
        }
        out.extend_from_slice(content);
        out
    }

    /// A distinguishable filler run of `len` bytes.
    ///
    /// # Arguments
    ///
    /// * `len` - How many bytes to produce
    fn filler(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 | 0x80).collect()
    }

    /// A minimal `AlgorithmIdentifier`: an OID plus a NULL parameter.
    fn algorithm_identifier() -> Vec<u8> {
        let mut inner = tlv(
            TAG_OID,
            &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01],
        );
        inner.extend_from_slice(&tlv(0x05, &[]));
        tlv(TAG_SEQUENCE, &inner)
    }

    /// A structurally valid `SubjectPublicKeyInfo`.
    fn spki() -> Vec<u8> {
        let mut inner = algorithm_identifier();
        // the BIT STRING's leading zero is its unused-bit count
        let mut key = vec![0x00];
        key.extend_from_slice(&filler(140));
        inner.extend_from_slice(&tlv(TAG_BIT_STRING, &key));
        tlv(TAG_SEQUENCE, &inner)
    }

    /// A structurally valid PKCS#1 `RSAPrivateKey`.
    fn rsa_private() -> Vec<u8> {
        let mut inner = tlv(TAG_INTEGER, &[0x00]);
        inner.extend_from_slice(&tlv(TAG_INTEGER, &filler(128)));
        inner.extend_from_slice(&tlv(TAG_INTEGER, &[0x01, 0x00, 0x01]));
        // d, p, q, dP, dQ, qInv
        for len in [128, 64, 64, 64, 64, 64] {
            inner.extend_from_slice(&tlv(TAG_INTEGER, &filler(len)));
        }
        tlv(TAG_SEQUENCE, &inner)
    }

    /// A well-formed `SubjectPublicKeyInfo` is recognized and sized exactly.
    #[test]
    fn identify_accepts_a_public_key_structure() {
        let der = spki();
        let found = identify(&der).expect("a valid SubjectPublicKeyInfo");
        assert_eq!(found.kind, DerKind::SubjectPublicKeyInfo);
        assert_eq!(found.at, 0);
        // the reported length is the whole object, header included
        assert_eq!(found.len, der.len());
        assert_eq!(found.kind.recovered_kind(), RecoveredKind::PublicKey);
        assert!(!found.kind.is_private());
    }

    /// The full RSA private parameter set is recognized as private material.
    #[test]
    fn identify_accepts_an_rsa_private_key() {
        let found = identify(&rsa_private()).expect("a valid RSAPrivateKey");
        assert_eq!(found.kind, DerKind::RsaPrivateKey);
        assert!(found.kind.is_private());
        assert_eq!(found.kind.recovered_kind(), RecoveredKind::PrivateKey);
    }

    /// An X.509 certificate is recognized, and its inner key is not re-reported.
    #[test]
    fn identify_accepts_a_certificate_without_reporting_its_inner_key() {
        // tbsCertificate: version, serial, signature, issuer, validity, subject, SPKI
        let mut tbs = tlv(TAG_CTX_0, &tlv(TAG_INTEGER, &[0x02]));
        tbs.extend_from_slice(&tlv(TAG_INTEGER, &filler(8)));
        tbs.extend_from_slice(&algorithm_identifier());
        tbs.extend_from_slice(&tlv(
            0x31,
            &tlv(TAG_SEQUENCE, &tlv(TAG_OID, &[0x55, 0x04, 0x03])),
        ));
        tbs.extend_from_slice(&spki());
        let mut cert = tlv(TAG_SEQUENCE, &tbs);
        cert.extend_from_slice(&algorithm_identifier());
        let mut signature = vec![0x00];
        signature.extend_from_slice(&filler(128));
        cert.extend_from_slice(&tlv(TAG_BIT_STRING, &signature));
        let der = tlv(TAG_SEQUENCE, &cert);
        let found = scan(&der);
        // exactly one object: the certificate, not the SPKI nested inside it
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, DerKind::Certificate);
        assert_eq!(found[0].len, der.len());
    }

    /// A truncated object is rejected rather than reported at its declared size.
    #[test]
    fn identify_rejects_a_truncated_object() {
        let der = spki();
        assert!(identify(&der[..der.len() - 8]).is_none());
    }

    /// Bytes left over inside the sequence break the tiling and the object is rejected.
    #[test]
    fn identify_rejects_children_that_do_not_tile() {
        let mut inner = algorithm_identifier();
        let mut key = vec![0x00];
        key.extend_from_slice(&filler(140));
        inner.extend_from_slice(&tlv(TAG_BIT_STRING, &key));
        // one stray byte the children cannot account for
        inner.push(0x2a);
        assert!(identify(&tlv(TAG_SEQUENCE, &inner)).is_none());
    }

    /// A non-minimal length encoding is not DER and is rejected.
    #[test]
    fn read_tlv_rejects_non_minimal_lengths() {
        // long form used for a length the short form expresses
        assert!(read_tlv(&[TAG_SEQUENCE, 0x81, 0x02, 0x00, 0x00], 0).is_none());
        // a leading zero octet in the length
        assert!(read_tlv(&[TAG_SEQUENCE, 0x82, 0x00, 0x80], 0).is_none());
        // the indefinite form, which DER forbids
        assert!(read_tlv(&[TAG_SEQUENCE, 0x80, 0x00, 0x00], 0).is_none());
        // and a well-formed long-form length is accepted
        let mut buf = vec![TAG_SEQUENCE, 0x81, 0x80];
        buf.extend_from_slice(&filler(0x80));
        let read = read_tlv(&buf, 0).expect("a minimal long-form length");
        assert_eq!(read.len, 0x80);
        assert_eq!(read.header, 3);
    }

    /// A valid but unrecognized structure is rejected — "some ASN.1" is not a finding.
    #[test]
    fn identify_rejects_an_unrecognized_structure() {
        // a well-formed SEQUENCE of octet strings: consistent, but not key material
        let mut inner = tlv(TAG_OCTET_STRING, &filler(40));
        inner.extend_from_slice(&tlv(TAG_OCTET_STRING, &filler(40)));
        assert!(identify(&tlv(TAG_SEQUENCE, &inner)).is_none());
    }

    /// Two small integers are not read as an RSA public key.
    #[test]
    fn identify_rejects_small_integer_pairs() {
        let mut inner = tlv(TAG_INTEGER, &filler(8));
        inner.extend_from_slice(&tlv(TAG_INTEGER, &filler(8)));
        // padded to clear the minimum-length floor so the shape check is what rejects it
        inner.extend_from_slice(&tlv(TAG_INTEGER, &filler(64)));
        assert!(identify(&tlv(TAG_SEQUENCE, &inner)).is_none());
    }

    /// A scan over surrounding binary finds the embedded object at its true offset.
    #[test]
    fn scan_locates_an_object_inside_a_data_blob() {
        let der = rsa_private();
        let mut buf = filler(37);
        let at = buf.len();
        buf.extend_from_slice(&der);
        buf.extend_from_slice(&filler(23));
        let found = scan(&buf);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].at, at);
        assert_eq!(found[0].len, der.len());
    }

    /// A run of ordinary data yields nothing.
    #[test]
    fn scan_is_quiet_on_ordinary_data() {
        // a byte ramp repeated, which contains many 0x30 bytes and plausible lengths
        let buf: Vec<u8> = (0..4096u32).map(|i| (i % 256) as u8).collect();
        assert!(scan(&buf).is_empty());
        // and ASCII text, where 0x30 is the digit '0'
        let text = "0123456789 the quick brown fox jumps over the lazy dog 0000000000"
            .repeat(40)
            .into_bytes();
        assert!(scan(&text).is_empty());
    }
}
