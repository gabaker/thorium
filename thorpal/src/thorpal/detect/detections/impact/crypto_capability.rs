//! Cryptographic capability detection (MITRE ATT&CK **T1486**).
//!
//! Surfaces evidence that a sample can encrypt data — the core of ransomware, but
//! also useful for spotting encrypted C2/exfil (T1573). Three angles, so at least
//! one fires even on stripped binaries or bespoke crypto:
//!
//! 1. **Crypto constants** (findcrypt-style) — well-known tables/magic numbers that
//!    betray a specific algorithm even with no crypto imports: AES S-boxes, SHA/MD5
//!    init + round constants, the ChaCha/Salsa `"expand 32-byte k"` nonce, the RC5/RC6
//!    key-schedule constants (P/Q), the CRC32 table, and base64 alphabets. Found by
//!    scanning section bytes.
//! 2. **Embedded keys / certs** — PEM boundaries for a baked-in public key or
//!    certificate (an attacker RSA public key used to wrap file session keys), and
//!    base64-encoded **symmetric keys** (RC6/AES) carved directly from raw section
//!    bytes. Found via defined strings and a raw-byte scan.
//! 3. **Crypto APIs** — Win32 CryptoAPI / CNG calls. Found via imports.
//!
//! NOTE: a constant-time/bitsliced AES (e.g. Rust's `aes` fixslice backend, which
//! Rust-built ELF samples in the measurement corpus link) has **no S-box table**, so
//! the S-box signatures will not fire there — the ChaCha/base64/PEM/API angles cover
//! that case.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::thorpal::detect::confidence::Confidence;
use anyhow::Result;

use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::decode::{self, RecoveredKind, der, recover};
use crate::thorpal::detect::detections::{
    ApiSpec, ArtifactClass, AttackTechnique, ByteSig, Detection, DetectionHit, EvidenceKind,
    HitArtifact, api_call_hits, high, is_library_function, low, medium, normalize, signature_hits,
};
use crate::thorpal::detect::indicators::match_runtime_marker;

use super::T1486;

/// The Crypto capability detection (ATT&CK T1486).
pub struct CryptoCapability;

/// Strong crypto constants — a match is near-conclusive evidence of the algorithm.
const STRONG_SIGS: &[ByteSig] = &[
    ByteSig {
        name: "ChaCha/Salsa20 nonce",
        always_noise: false,
        // the ASCII sigma constant "expand 32-byte k" used by ChaCha20/Salsa20
        bytes: b"expand 32-byte k",
        reasoning: "Contains the ChaCha20/Salsa20 constant \"expand 32-byte k\"; this stream cipher \
                    is used for file/session encryption (and CSPRNGs like rand_chacha). Strong \
                    evidence of a real crypto implementation.",
    },
    ByteSig {
        name: "AES forward S-box",
        always_noise: false,
        // first 32 bytes of the AES (Rijndael) forward S-box
        bytes: &[
            0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7,
            0xab, 0x76, 0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf,
            0x9c, 0xa4, 0x72, 0xc0,
        ],
        reasoning: "Contains the AES (Rijndael) forward S-box table; conclusive evidence of a \
                    table-based AES implementation used for bulk data encryption.",
    },
    ByteSig {
        name: "AES inverse S-box",
        always_noise: false,
        // first 32 bytes of the AES inverse S-box (decryption)
        bytes: &[
            0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38, 0xbf, 0x40, 0xa3, 0x9e, 0x81, 0xf3,
            0xd7, 0xfb, 0x7c, 0xe3, 0x39, 0x82, 0x9b, 0x2f, 0xff, 0x87, 0x34, 0x8e, 0x43, 0x44,
            0xc4, 0xde, 0xe9, 0xcb,
        ],
        reasoning: "Contains the AES inverse S-box table (used for decryption); conclusive evidence \
                    of a table-based AES implementation.",
    },
];

/// Word-oriented crypto constants (init vectors / round constants / tables).
const WORD_SIGS: &[ByteSig] = &[
    ByteSig {
        name: "SHA-256 round constants (BE)",
        always_noise: false,
        // first four 32-bit K constants, big-endian
        bytes: &[
            0x42, 0x8a, 0x2f, 0x98, 0x71, 0x37, 0x44, 0x91, 0xb5, 0xc0, 0xfb, 0xcf, 0xe9, 0xb5,
            0xdb, 0xa5,
        ],
        reasoning: "Contains the SHA-256 round-constant table (K); evidence of SHA-256 hashing, \
                    commonly a key-derivation or integrity step in a crypto/ransomware routine.",
    },
    ByteSig {
        name: "SHA-256 round constants (LE)",
        always_noise: false,
        // first four 32-bit K constants, little-endian (x86 word storage)
        bytes: &[
            0x98, 0x2f, 0x8a, 0x42, 0x91, 0x44, 0x37, 0x71, 0xcf, 0xfb, 0xc0, 0xb5, 0xa5, 0xdb,
            0xb5, 0xe9,
        ],
        reasoning: "Contains the SHA-256 round-constant table (K), little-endian word storage; \
                    evidence of SHA-256 hashing.",
    },
    ByteSig {
        name: "SHA-256 init hash (BE)",
        always_noise: false,
        // first four 32-bit H init values, big-endian
        bytes: &[
            0x6a, 0x09, 0xe6, 0x67, 0xbb, 0x67, 0xae, 0x85, 0x3c, 0x6e, 0xf3, 0x72, 0xa5, 0x4f,
            0xf5, 0x3a,
        ],
        reasoning: "Contains the SHA-256 initial hash values (H); evidence of SHA-256 hashing.",
    },
    ByteSig {
        name: "SHA-256 init hash (LE)",
        always_noise: false,
        // first four 32-bit H init values, little-endian
        bytes: &[
            0x67, 0xe6, 0x09, 0x6a, 0x85, 0xae, 0x67, 0xbb, 0x72, 0xf3, 0x6e, 0x3c, 0x3a, 0xf5,
            0x4f, 0xa5,
        ],
        reasoning: "Contains the SHA-256 initial hash values (H), little-endian; evidence of \
                    SHA-256 hashing.",
    },
    ByteSig {
        name: "MD5 sine table (BE)",
        always_noise: false,
        // first four 32-bit MD5 T[] sine constants, big-endian
        bytes: &[
            0xd7, 0x6a, 0xa4, 0x78, 0xe8, 0xc7, 0xb7, 0x56, 0x24, 0x20, 0x70, 0xdb, 0xc1, 0xbd,
            0xce, 0xee,
        ],
        reasoning: "Contains the MD5 T[] sine-constant table; evidence of MD5 hashing.",
    },
    ByteSig {
        name: "MD5 sine table (LE)",
        always_noise: false,
        // first four 32-bit MD5 T[] sine constants, little-endian
        bytes: &[
            0x78, 0xa4, 0x6a, 0xd7, 0x56, 0xb7, 0xc7, 0xe8, 0xdb, 0x70, 0x20, 0x24, 0xee, 0xce,
            0xbd, 0xc1,
        ],
        reasoning: "Contains the MD5 T[] sine-constant table, little-endian; evidence of MD5 hashing.",
    },
    ByteSig {
        name: "SHA-1 init hash (BE)",
        always_noise: false,
        // the five 32-bit SHA-1 init constants (fifth word disambiguates from MD5)
        bytes: &[
            0x67, 0x45, 0x23, 0x01, 0xef, 0xcd, 0xab, 0x89, 0x98, 0xba, 0xdc, 0xfe, 0x10, 0x32,
            0x54, 0x76, 0xc3, 0xd2, 0xe1, 0xf0,
        ],
        reasoning: "Contains the SHA-1 initial hash values; evidence of SHA-1 hashing.",
    },
    ByteSig {
        name: "SHA-1 init hash (LE)",
        always_noise: false,
        // the five 32-bit SHA-1 init constants, little-endian
        bytes: &[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10, 0xf0, 0xe1, 0xd2, 0xc3,
        ],
        reasoning: "Contains the SHA-1 initial hash values, little-endian; evidence of SHA-1 hashing.",
    },
    ByteSig {
        name: "RC5/RC6 magic constant P (LE)",
        always_noise: false,
        // the RC5/RC6 key-schedule constant P = 0xB7E15163, little-endian (x86 word storage)
        bytes: &[0x63, 0x51, 0xe1, 0xb7],
        reasoning: "Contains the RC5/RC6 key-schedule magic constant P (0xB7E15163, derived from \
                    Euler's number e); the seed of the RC5/RC6 round-key table. Evidence of an \
                    RC5/RC6 block cipher used for payload/config or file encryption.",
    },
    ByteSig {
        name: "RC5/RC6 magic constant P (BE)",
        always_noise: false,
        // the RC5/RC6 key-schedule constant P = 0xB7E15163, big-endian
        bytes: &[0xb7, 0xe1, 0x51, 0x63],
        reasoning: "Contains the RC5/RC6 key-schedule magic constant P (0xB7E15163, derived from \
                    Euler's number e); the seed of the RC5/RC6 round-key table. Evidence of an \
                    RC5/RC6 block cipher used for payload/config or file encryption.",
    },
];

/// Weak/ubiquitous crypto-adjacent constants (also common in benign code).
const WEAK_SIGS: &[ByteSig] = &[
    ByteSig {
        name: "base64 standard alphabet",
        // every base64 encoder ever linked carries this table; the seeder may
        // roll these hits up as noise regardless of toolchain
        always_noise: true,
        bytes: b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
        reasoning: "Contains the standard base64 alphabet; base64 frequently wraps encrypted keys/\
                    payloads, including cipher data embedded in a dropped note. Ubiquitous, \
                    hence low confidence.",
    },
    ByteSig {
        name: "base64 URL-safe alphabet",
        // every URL-safe base64 encoder carries this table; the seeder may roll
        // these hits up as noise regardless of toolchain
        always_noise: true,
        bytes: b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
        reasoning: "Contains the URL-safe base64 alphabet; used to wrap encrypted keys/payloads for \
                    transport. Ubiquitous, hence low confidence.",
    },
    ByteSig {
        name: "RC5/RC6/TEA magic constant Q (LE)",
        always_noise: false,
        // the RC5/RC6 key-schedule constant Q = 0x9E3779B9, little-endian
        bytes: &[0xb9, 0x79, 0x37, 0x9e],
        reasoning: "Contains the golden-ratio constant Q (0x9E3779B9) used by the RC5/RC6 key \
                    schedule; corroborates an RC5/RC6 cipher when seen alongside the P constant. \
                    Also used by the TEA/XTEA cipher family and many hash mixers, hence low \
                    confidence on its own.",
    },
    ByteSig {
        name: "RC5/RC6/TEA magic constant Q (BE)",
        always_noise: false,
        // the RC5/RC6 key-schedule constant Q = 0x9E3779B9, big-endian
        bytes: &[0x9e, 0x37, 0x79, 0xb9],
        reasoning: "Contains the golden-ratio constant Q (0x9E3779B9) used by the RC5/RC6 key \
                    schedule; corroborates an RC5/RC6 cipher when seen alongside the P constant. \
                    Also used by the TEA/XTEA cipher family and many hash mixers, hence low \
                    confidence on its own.",
    },
];

/// The signature-name prefix both P-constant [`WORD_SIGS`] entries share.
///
/// A scanned hit's content leads with its signature name (see
/// `detections::build_signature_hit`), so this prefix identifies the P hits in the
/// scan results without re-walking the sections.
const P_SIG_PREFIX: &str = "RC5/RC6 magic constant P";

/// The signature-name prefix both Q-constant [`WEAK_SIGS`] entries share.
const Q_SIG_PREFIX: &str = "RC5/RC6/TEA magic constant Q";

/// The reasoning note appended to a Q-constant hit when the P constant is absent
/// image-wide (see [`annotate_p_less_q_hits`]).
const Q_WITHOUT_P_NOTE: &str = "\n\nThe RC5/RC6 companion constant P (0xB7E15163) is absent from this image, so this \
     match does not corroborate an RC5/RC6 key schedule. A lone 0x9E3779B9 is most often \
     the golden-ratio hash mixer (e.g. the 64-bit Fibonacci-hashing multiplier \
     0x9E3779B97F4A7C15 that Rust hash tables bake into code as an immediate operand), a \
     PRNG seed, or a coincidental 4-byte match inside compressed data. The TEA/XTEA cipher \
     family does use this delta with no P constant, so a cluster of these matches near \
     loop code is still worth a look as a TEA payload/config decryptor before the match \
     is written off.";

/// Append the P-absence framing note to every Q-constant hit, in place.
///
/// The Q signature's own reasoning says it corroborates RC5/RC6 only alongside the P
/// constant, yet the 4-byte pattern also matches hash mixers, PRNG seeds, and
/// compressed streams. When P is absent image-wide the hit still seeds (TEA/XTEA
/// genuinely use the Q delta and never P), but its reasoning states exactly what a
/// P-less match does and does not support, and the hit is marked always-noise so
/// the seeder treats it as rollup-eligible regardless of toolchain.
///
/// # Arguments
///
/// * `hits` - The scanned [`WEAK_SIGS`] hits to annotate
fn annotate_p_less_q_hits(hits: &mut [DetectionHit]) {
    for hit in hits {
        // only the Q-constant hits carry the RC5/RC6 corroboration claim
        if hit.content.starts_with(Q_SIG_PREFIX) {
            hit.reasoning.push_str(Q_WITHOUT_P_NOTE);
            // a P-less Q is overwhelmingly a hash mixer/PRNG seed, so the seeder
            // may roll it up as noise regardless of toolchain
            hit.always_noise = true;
        }
    }
}

/// PEM boundary substrings (lower-case) that reveal an embedded key or certificate in a
/// defined string. Used by [`pem_hits`] to seed a full-block recovery from each boundary.
const PEM_STRINGS: &[&str] = &[
    "-----begin public key",
    "-----begin rsa",
    "-----begin private key",
    "-----begin certificate",
    "-----begin ",
];

/// The analyst reasoning for a PEM boundary present in a defined string whose full block
/// could not be recovered (e.g. a runtime `encoding/pem` template, not a baked-in key).
const PEM_MARKER_REASON: &str = "Contains a PEM boundary (-----BEGIN ... KEY/CERTIFICATE-----), i.e. an embedded key \
     or certificate. Ransomware bakes in an attacker RSA public key to wrap the file \
     session key; it can also indicate pinned C2 crypto.";

/// Cryptographic APIs (Win32 `CryptoAPI` / CNG).
///
/// The reasonings are deliberately capability-neutral: every one of these APIs is
/// dual-use (C2 channel crypto, config decryption, hashing schemes, and exfil
/// transport in the measured corpus, as often as file encryption), so each names
/// the capability and the candidate roles rather than asserting a ransomware role
/// the call site alone cannot support.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "CryptEncrypt",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls CryptEncrypt (Win32 CryptoAPI) to encrypt data — a dual-use capability \
                    used for victim-file encryption, C2/exfil protection, or wrapping \
                    configs/payloads. Attribute it to T1486 only when file-enumeration and \
                    file-rewrite evidence corroborates a file cryptor.",
    },
    ApiSpec {
        key: "BCryptEncrypt",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls BCryptEncrypt (CNG) to encrypt data — a dual-use capability used for \
                    victim-file encryption, C2/exfil protection, or wrapping configs/payloads. \
                    Attribute it to T1486 only when file-walk evidence corroborates a file \
                    cryptor.",
    },
    ApiSpec {
        key: "CryptDecrypt",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls CryptDecrypt (Win32 CryptoAPI) to decrypt data — commonly an embedded \
                    payload/config decryption or the receive half of an encrypted C2 channel; \
                    it can also be the decrypt half of a file cryptor.",
    },
    ApiSpec {
        key: "CryptImportKey",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls CryptImportKey to load a key blob. Key import is dual-use — C2 \
                    session-key exchange, config decryption, and signature checks as well as \
                    public-key wrapping of file-encryption keys — so which role applies depends \
                    on where the blob comes from and what the keyed output feeds.",
    },
    ApiSpec {
        key: "CryptGenKey",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls CryptGenKey to generate a symmetric/session key — a transport key for \
                    an encrypted C2 channel as often as a file-encryption key; generation alone \
                    does not say which role the key plays.",
    },
    ApiSpec {
        key: "CryptAcquireContext",
        // the setup call before ANY CryptoAPI use proves nothing by itself; the
        // seeder may roll these hits up as noise regardless of toolchain
        always_noise: true,
        severity: low(),
        reasoning: "Calls CryptAcquireContext to open a CryptoAPI provider — the setup step \
                    before any CryptoAPI use (hashing, random bytes, signatures, or \
                    encryption). Ubiquitous and capability-neutral, hence low confidence.",
    },
    ApiSpec {
        key: "BCryptGenerateSymmetricKey",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls BCryptGenerateSymmetricKey (CNG) to create a symmetric key — used for \
                    file encryption, C2/exfil protection, or config decryption; the key's \
                    consumer, not its creation, determines the role.",
    },
    ApiSpec {
        key: "CryptStringToBinary",
        // routine certificate/config decoding in benign and malicious code alike;
        // the seeder may roll these hits up as noise regardless of toolchain
        always_noise: true,
        severity: low(),
        reasoning: "Calls CryptStringToBinary to decode base64/hex text into raw bytes — \
                    routinely certificate/key parsing or config decoding ahead of the crypto \
                    APIs, in benign and malicious code alike.",
    },
];

/// An imported API that consumes key material — importing, parsing, or decoding a
/// key blob, certificate, or PEM/DER structure.
///
/// A recovered key is only actionable once it names the code that uses it, so this
/// table exists purely to attribute a recovery. It never scores anything on its own.
struct KeyConsumer {
    /// The normalized import name to match (see [`normalize`]).
    key: &'static str,
    /// How the function is described as using the key material.
    label: &'static str,
}

/// Imported key-import / key-parse APIs, across the common crypto stacks.
///
/// Covers the Win32 CryptoAPI/CNG surface, OpenSSL's PEM/DER entry points, and
/// mbedTLS's parsers, because which stack a sample links is a build choice with no
/// bearing on whether its embedded key matters.
const KEY_CONSUMERS: &[KeyConsumer] = &[
    // --- Win32 CryptoAPI / CNG ---
    KeyConsumer {
        key: "CryptImportKey",
        label: "CryptImportKey",
    },
    KeyConsumer {
        key: "CryptImportPublicKeyInfo",
        label: "CryptImportPublicKeyInfo",
    },
    KeyConsumer {
        key: "CryptDecodeObjectEx",
        label: "CryptDecodeObjectEx",
    },
    KeyConsumer {
        key: "CryptStringToBinary",
        label: "CryptStringToBinary",
    },
    KeyConsumer {
        key: "CryptDeriveKey",
        label: "CryptDeriveKey",
    },
    KeyConsumer {
        key: "BCryptImportKeyPair",
        label: "BCryptImportKeyPair",
    },
    KeyConsumer {
        key: "BCryptImportKey",
        label: "BCryptImportKey",
    },
    KeyConsumer {
        key: "NCryptImportKey",
        label: "NCryptImportKey",
    },
    KeyConsumer {
        key: "CertCreateCertificateContext",
        label: "CertCreateCertificateContext",
    },
    // --- OpenSSL / LibreSSL ---
    KeyConsumer {
        key: "PEM_read_bio_PUBKEY",
        label: "PEM_read_bio_PUBKEY",
    },
    KeyConsumer {
        key: "PEM_read_bio_PrivateKey",
        label: "PEM_read_bio_PrivateKey",
    },
    KeyConsumer {
        key: "PEM_read_bio_X509",
        label: "PEM_read_bio_X509",
    },
    KeyConsumer {
        key: "d2i_PUBKEY",
        label: "d2i_PUBKEY",
    },
    KeyConsumer {
        key: "d2i_RSA_PUBKEY",
        label: "d2i_RSA_PUBKEY",
    },
    KeyConsumer {
        key: "d2i_PrivateKey",
        label: "d2i_PrivateKey",
    },
    KeyConsumer {
        key: "d2i_X509",
        label: "d2i_X509",
    },
    KeyConsumer {
        key: "EVP_PKEY_new_raw_public_key",
        label: "EVP_PKEY_new_raw_public_key",
    },
    KeyConsumer {
        key: "EVP_PKEY_new_raw_private_key",
        label: "EVP_PKEY_new_raw_private_key",
    },
    // --- mbedTLS ---
    KeyConsumer {
        key: "mbedtls_pk_parse_public_key",
        label: "mbedtls_pk_parse_public_key",
    },
    KeyConsumer {
        key: "mbedtls_pk_parse_key",
        label: "mbedtls_pk_parse_key",
    },
    KeyConsumer {
        key: "mbedtls_x509_crt_parse",
        label: "mbedtls_x509_crt_parse",
    },
];

/// A runtime symbol marker naming a function that parses key material in a
/// statically linked binary.
///
/// A Go or Rust binary has no import table, so the key-parse step is only visible
/// as a linked-in symbol name. Without this the entire attribution above would
/// silently do nothing on exactly the binaries that hide their imports.
struct KeyRuntime {
    /// The lower-case symbol substring to match.
    marker: &'static str,
    /// How the function is described as using the key material.
    label: &'static str,
}

/// Runtime key-parse symbol markers, in precedence order.
const KEY_RUNTIME: &[KeyRuntime] = &[
    // --- Go standard library ---
    KeyRuntime {
        marker: "x509.parsepkixpublickey",
        label: "crypto/x509 public-key parse",
    },
    KeyRuntime {
        marker: "x509.parsecertificate",
        label: "crypto/x509 certificate parse",
    },
    KeyRuntime {
        marker: "x509.parsepkcs1",
        label: "crypto/x509 PKCS#1 key parse",
    },
    KeyRuntime {
        marker: "x509.parsepkcs8",
        label: "crypto/x509 PKCS#8 key parse",
    },
    KeyRuntime {
        marker: "x509.parseeckey",
        label: "crypto/x509 EC key parse",
    },
    KeyRuntime {
        marker: "pem.decode",
        label: "encoding/pem decode",
    },
    KeyRuntime {
        marker: "rsa.encryptoaep",
        label: "crypto/rsa OAEP encrypt",
    },
    KeyRuntime {
        marker: "rsa.encryptpkcs1v15",
        label: "crypto/rsa PKCS#1 encrypt",
    },
    KeyRuntime {
        marker: "aes.newcipher",
        label: "crypto/aes cipher setup",
    },
    KeyRuntime {
        marker: "cipher.newgcm",
        label: "crypto/cipher GCM setup",
    },
    // --- Rust crates ---
    KeyRuntime {
        marker: "x509_parser",
        label: "x509-parser certificate parse",
    },
    KeyRuntime {
        marker: "pkcs8::",
        label: "pkcs8 key parse",
    },
    KeyRuntime {
        marker: "spki::",
        label: "spki public-key parse",
    },
    KeyRuntime {
        marker: "rsa::",
        label: "rsa crate key handling",
    },
    KeyRuntime {
        marker: "aes_gcm",
        label: "aes-gcm cipher setup",
    },
    KeyRuntime {
        marker: "chacha20poly1305",
        label: "chacha20poly1305 cipher setup",
    },
    KeyRuntime {
        marker: "ring::signature",
        label: "ring signature key handling",
    },
];

/// Multi-word PEM label phrases that mark a block as deliberately non-production.
///
/// A standard library that ships test key material renames the armor rather than
/// hiding it — Go's `crypto/tls` emits `-----BEGIN RSA TESTING KEY-----` precisely
/// so a scanner can tell the difference. Matching is on the whole phrase, never on
/// a bare `test` substring, because that would also strike a real key stored beside
/// any word containing it.
const TEST_LABELS: &[&str] = &[
    "testing key",
    "test key",
    "example key",
    "sample key",
    "dummy key",
    "test certificate",
    "testing certificate",
    "example certificate",
    "snakeoil",
];

#[async_trait::async_trait]
impl Detection for CryptoCapability {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "crypto_capability"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1486
    }

    /// Run the crypto-constant, embedded-key, and crypto-API passes.
    #[tracing::instrument(name = "CryptoCapability::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();

        // resolve, once, which functions parse or import key material — this is what
        // every key recovery below is attributed against, so a recovered key names the
        // code that consumes it rather than floating free at an address
        let consumers = key_consumers(ctx).await?;

        // 1) crypto constants, tiered by how conclusive a match is
        hits.extend(
            signature_hits(ctx, T1486, STRONG_SIGS, high(), EvidenceKind::ContextOnly).await?,
        );
        let word_hits =
            signature_hits(ctx, T1486, WORD_SIGS, medium(), EvidenceKind::ContextOnly).await?;
        // whether the RC5/RC6 P constant landed anywhere in the image: the Q constant
        // corroborates an RC5/RC6 key schedule only alongside P
        let p_present = word_hits
            .iter()
            .any(|hit| hit.content.starts_with(P_SIG_PREFIX));
        hits.extend(word_hits);
        let mut weak_hits =
            signature_hits(ctx, T1486, WEAK_SIGS, low(), EvidenceKind::ContextOnly).await?;
        // P-less Q matches keep seeding (TEA/XTEA use the Q delta with no P), but say
        // exactly what they do and do not support
        if !p_present {
            annotate_p_less_q_hits(&mut weak_hits);
        }
        hits.extend(weak_hits);

        // 2) embedded PEM keys/certificates: recover the FULL block by locating both
        //    boundaries in the raw section bytes, seeded from both a raw-byte scan and any
        //    PEM boundary in a defined string, deduped so neither mechanism double-reports.
        //    The same pass also decodes long base64 blobs and re-scans their output, so a
        //    key stored behind an encoding layer is recovered rather than missed
        hits.extend(pem_hits(ctx, &consumers).await?);

        // 3) unarmored key material: DER/ASN.1 structures stored as raw bytes, which the
        //    boundary scan above cannot see because they announce nothing
        hits.extend(der_hits(ctx, &consumers).await?);

        // 4) crypto API call sites
        hits.extend(api_call_hits(ctx, T1486, APIS, EvidenceKind::ApiResultCheck).await?);

        // 5) best-effort static decryption: recover embedded key + ciphertext and
        //    replay RC4 / RC6 / single-byte streams to surface the plaintext
        hits.extend(crypto_decrypt_hits(ctx).await?);

        // 6) embedded symmetric key material: base64, key-length, high-entropy blobs
        //    carved directly from the raw section bytes (the Go/packed-blob case that
        //    the disassembler never splits into a discrete string)
        hits.extend(embedded_key_hits(ctx, &consumers).await?);

        Ok(hits)
    }
}

/// The functions that consume key material, keyed by entry address.
///
/// The value is the set of consumer labels seen in that function, so a recovery
/// attributed to it can name what the code actually does with the key rather than
/// asserting a role the evidence does not support.
type KeyConsumers = HashMap<u64, BTreeSet<&'static str>>;

/// Locate every function that imports, parses, or decodes key material.
///
/// Two mechanisms, because a stripped statically linked binary has neither an
/// import table nor a symbol for the API it would otherwise call:
///
/// * **imports** — each [`KEY_CONSUMERS`] entry's call sites resolved back to their
///   callers, hopping import thunks so the attribution lands on real code.
/// * **linked runtimes** — a function whose own name matches a [`KEY_RUNTIME`]
///   marker *is* the parsing routine.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if resolving an import's call sites fails.
#[tracing::instrument(name = "crypto_capability::key_consumers", skip_all, err(Debug))]
async fn key_consumers(ctx: &AnalysisContext) -> Result<KeyConsumers> {
    use anyhow::Context as _;
    let mut found: KeyConsumers = HashMap::new();
    // --- imported key-import/parse APIs: attribute each call site to its caller ---
    for import in &ctx.imports {
        let norm = normalize(&import.name);
        let Some(spec) = KEY_CONSUMERS.iter().find(|spec| spec.key == norm) else {
            continue;
        };
        let sites = ctx
            .resolve_call_sites(&import.address)
            .await
            .with_context(|| format!("resolving call sites of import '{}'", import.name))?;
        for site in sites {
            // a call site outside any function names no consumer
            let Some(entry) = site.function else {
                continue;
            };
            found.entry(entry).or_default().insert(spec.label);
        }
    }
    // --- linked runtimes: the parsing function is the marked symbol itself ---
    for function in &ctx.functions {
        let Some(marker) = match_runtime_marker(&function.name, KEY_RUNTIME, |m| m.marker) else {
            continue;
        };
        let Some(entry) = crate::thorpal::detect::context::parse_addr(&function.entry_point)
            .or_else(|| crate::thorpal::detect::context::parse_addr(&function.address))
        else {
            continue;
        };
        found.entry(entry).or_default().insert(marker.label);
    }
    Ok(found)
}

/// Build the artifact attribution for a recovered key: the role phrase naming the
/// **observed** relationship, plus a detail line when the attribution is partial or
/// absent.
///
/// Three outcomes, and the difference between them is what an analyst needs:
/// a referencing function that also parses key material is a genuine consumer; a
/// referencing function that does not is only a reference; and no referencing
/// function at all must read as unattributed rather than silently look attributed.
///
/// # Arguments
///
/// * `consumers` - The key-consuming functions, from [`key_consumers`]
/// * `function` - The entry of the function that references the artifact, if found
/// * `stored` - How the stored form is described (e.g. `"PEM block"`, `"DER structure"`)
/// * `window` - The reference window the attribution search used, for the failure note
fn key_attribution(
    consumers: &KeyConsumers,
    function: Option<u64>,
    stored: &str,
    window: u64,
) -> (Option<String>, Option<String>) {
    match function {
        Some(entry) => match consumers.get(&entry) {
            // the referencing function also parses key material — an observed consumer
            Some(labels) => (
                Some(format!(
                    "references this {stored} and passes key material to {}",
                    labels.iter().copied().collect::<Vec<_>>().join(", ")
                )),
                None,
            ),
            // referenced, but nothing in it parses a key: say only what was seen
            None => (
                Some(format!("references this {stored}")),
                Some(
                    "no key-import or key-parse call was observed in the referencing function; \
                     the data reference is the whole of the attribution"
                        .to_string(),
                ),
            ),
        },
        // no code reference within the window — report it, but never imply an owner
        None => (
            None,
            Some(format!(
                "no function references this {stored} within {window} bytes, so its user was \
                 not located; the material is present in the image regardless"
            )),
        ),
    }
}

/// The max bytes of a candidate ciphertext buffer to decrypt (bounds cost).
const MAX_CT_BYTES: usize = 16 * 1024;
/// The max distinct candidate keys tried (printable embedded strings).
const MAX_KEYS: usize = 64;
/// The max distinct candidate ciphertext buffers tried.
const MAX_CTS: usize = 64;
/// The max decryption findings surfaced (keeps the report sane).
const MAX_DECRYPT_HITS: usize = 10;
/// The minimum length of a candidate embedded key.
const MIN_KEY_LEN: usize = 4;
/// The maximum length of a candidate embedded key.
const MAX_KEY_LEN: usize = 64;
/// The min length of a candidate ciphertext buffer.
const MIN_CT_BYTES: usize = 24;

/// A candidate key: its raw bytes plus a display rendering and where it lives.
struct KeyCandidate {
    /// The key bytes fed to the cipher.
    bytes: Vec<u8>,
    /// A human rendering for the flag (`"…"` if printable, else hex).
    display: String,
    /// The address the key string is stored at (0 when unknown).
    addr: u64,
    /// The entry of the function that references the key's buffer, when known (what
    /// the key↔ciphertext affinity check compares).
    function: Option<u64>,
}

/// A candidate ciphertext buffer: where it came from and its bytes.
struct CtCandidate {
    /// The address the ciphertext starts at (anchors the flag's site), 0 if unknown.
    addr: u64,
    /// The entry of the function that references this buffer, when known (so the flag
    /// links to the decrypting function rather than dangling on a data address).
    function: Option<u64>,
    /// The section the ciphertext came from (e.g. `.rodata`, or `base64 string`).
    section: String,
    /// The (bounded) ciphertext bytes.
    bytes: Vec<u8>,
}

/// Best-effort static decryption of embedded payloads.
///
/// Gathers candidate keys (printable embedded strings) and candidate ciphertext
/// buffers (mostly-non-printable data buffers the code references, plus base64-
/// unwrapped blobs), then replays **RC4** and single-byte streams over each pair.
/// Only decodes that yield genuinely-hidden readable text (guarded by
/// [`recover::hidden_runs`], plus the stricter repeating-XOR gate in
/// [`recovered_runs`]) are reported. The replay tries every key against every
/// buffer, so a surviving pair whose key and ciphertext share no referencing
/// function and sit nowhere near each other is demoted to the weak grade — the
/// pairing exists only because the brute force tried it — and the key rides as a
/// [`ArtifactClass::SymmetricKey`] artifact only on key-class recoveries
/// ([`carries_key_artifact`]), so a plain recovered string never pivot-shields its
/// hit. Everything is bounded so this stays cheap.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if disassembling the program or reading its sections fails.
#[tracing::instrument(name = "crypto_capability::crypto_decrypt_hits", skip_all, err(Debug))]
async fn crypto_decrypt_hits(ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
    // the data-reference index names the function referencing each key string, which
    // is one half of the key↔ciphertext affinity test below
    let index = ctx.reference_index().await?;
    // collect candidate keys and ciphertext buffers (both bounded)
    let keys = collect_keys(ctx, &index);
    let cts = collect_ciphertexts(ctx).await?;
    if keys.is_empty() || cts.is_empty() {
        return Ok(Vec::new());
    }

    let mut hits = Vec::new();
    // avoid surfacing the same recovered plaintext twice
    let mut seen: HashSet<String> = HashSet::new();
    'ct: for ct in &cts {
        // try each key against this ciphertext with each keyed stream scheme.
        // RC4's keystream is pseudo-random (no key-echo risk); repeating XOR echoes
        // the key through low-entropy/zero regions, so it is guarded against that.
        for key in &keys {
            // the keyed schemes to replay for this (ciphertext, key) pair. RC4 and
            // repeating XOR always apply; RC6 (a 16-byte block cipher) is added only for a
            // valid RC6 key length (16/24/32 bytes) to bound cost and stay meaningful. Its
            // keystream is pseudo-random for a wrong key, so — like RC4 — it needs no
            // key-echo guard (None).
            let mut schemes: Vec<SchemeAttempt> = vec![
                ("RC4", decode::rc4::decrypt(&key.bytes, &ct.bytes), None),
                (
                    "repeating XOR",
                    decode::xor::keystream(&key.bytes, &ct.bytes),
                    Some(key.bytes.as_slice()),
                ),
            ];
            if matches!(key.bytes.len(), 16 | 24 | 32) {
                schemes.push(("RC6", decode::rc6::decrypt_ecb(&key.bytes, &ct.bytes), None));
                schemes.push((
                    "RC6-CBC",
                    decode::rc6::decrypt_cbc(&key.bytes, &ct.bytes),
                    None,
                ));
            }
            for (scheme, plain, guard) in schemes {
                let Some(runs) = recovered_runs(&plain, &ct.bytes, guard) else {
                    continue;
                };
                // whether anything in the program ties this key to this buffer beyond
                // the replay itself; a brute-forced pairing with no static affinity is
                // where fabricated recoveries come from, so it reports quietly
                let affine = key_ct_affinity(key, ct);
                // one typed flag per recovered item (a PEM armor coalesced, trailing split)
                for (off, text) in recover::extract_items(runs) {
                    if !seen.insert(text.clone()) {
                        continue;
                    }
                    let addr = ct.addr.saturating_add(off as u64);
                    let kind = decode::classify(&text);
                    let name = decode::recovered_name(kind, &text, &ct.section, addr);
                    let content = decode::recovered_content(
                        kind,
                        &text,
                        &ct.section,
                        addr,
                        scheme,
                        Some(&key.display),
                    );
                    // grade the recovery, demoting a no-affinity pairing to the weak
                    // grade with a reasoning that names the missing tie
                    let (severity, reasoning) =
                        decrypt_severity(affine, kind, &text, scheme, &key.display);
                    let mut hit =
                        DetectionHit::new(T1486, addr, ct.function, severity, content, reasoning)
                            .name(name)
                            .kind(EvidenceKind::ContextOnly);
                    // the key rides as a pivotable artifact only when the pair has
                    // affinity AND the recovery is itself key material: artifact
                    // classes are pivot-class (they veto runtime weakening and rollup
                    // suppression), which a plain recovered string has no claim to
                    if affine && carries_key_artifact(kind) {
                        let mut artifact =
                            HitArtifact::new(ArtifactClass::SymmetricKey, key.display.clone())
                                .detail(format!(
                                    "recovered by replaying {scheme} over the buffer at {}",
                                    crate::thorpal::detect::context::fmt_addr(ct.addr)
                                ));
                        artifact = match ct.function {
                            Some(_) => artifact.role(format!(
                                "decrypts the buffer at {} with this key ({scheme})",
                                crate::thorpal::detect::context::fmt_addr(ct.addr)
                            )),
                            // the buffer decrypted, but no function was seen referencing it
                            None => artifact,
                        };
                        hit = hit.artifact(artifact);
                    }
                    hits.push(hit);
                    if hits.len() >= MAX_DECRYPT_HITS {
                        break 'ct;
                    }
                }
                // this ciphertext decrypted with this key/scheme; move to the next buffer
                continue 'ct;
            }
        }
    }
    Ok(hits)
}

/// Gather candidate embedded keys: printable defined strings of a plausible length.
///
/// Each candidate records where its string is stored and which function references
/// that buffer, so [`crypto_decrypt_hits`] can test whether a key and a ciphertext
/// have any static affinity before believing a replay that paired them.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `index` - The sorted data-reference index (to attribute each key string)
fn collect_keys(ctx: &AnalysisContext, index: &[(u64, u64)]) -> Vec<KeyCandidate> {
    let mut keys = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for string in &ctx.strings {
        // stop once the candidate budget is spent
        if keys.len() >= MAX_KEYS {
            break;
        }
        let bytes = string.value.as_bytes();
        // a plausible key: short, and fully printable (an embedded password/key)
        let len_ok = (MIN_KEY_LEN..=MAX_KEY_LEN).contains(&bytes.len());
        if !len_ok || !bytes.iter().all(|b| recover::is_printable(*b)) {
            continue;
        }
        // where the key string sits and which function references it, for the
        // key↔ciphertext affinity check
        let addr = crate::thorpal::detect::context::parse_addr(&string.address).unwrap_or(0);
        let function = decode::buffers::referencing_function(index, addr, KEY_REF_WINDOW);
        // 1) the raw string bytes as the key (an embedded plaintext password/key)
        push_key(
            &mut keys,
            &mut seen,
            bytes.to_vec(),
            format!("{:?}", string.value),
            addr,
            function,
        );
        // 2) a base64-wrapped key: unwrap it to its raw bytes and try those too, when the
        //    string is pure base64 that decodes to a standard symmetric key length
        //    (16/24/32 bytes) — e.g. an RC6/AES key stored as base64
        if let Some(decoded) = base64_key_bytes(&string.value) {
            let display = format!(
                "base64 {} → {} bytes",
                short_value(&string.value),
                decoded.len()
            );
            push_key(&mut keys, &mut seen, decoded, display, addr, function);
        }
    }
    keys
}

/// Push a candidate key, de-duplicated by its raw bytes.
///
/// # Arguments
///
/// * `keys` - The accumulating candidate list
/// * `seen` - The set of already-collected key material (dedup)
/// * `bytes` - The candidate key bytes
/// * `display` - A human rendering of the key for the flag
/// * `addr` - The address the key's string is stored at (0 when unknown)
/// * `function` - The entry of the function that references the key's buffer, if any
fn push_key(
    keys: &mut Vec<KeyCandidate>,
    seen: &mut HashSet<Vec<u8>>,
    bytes: Vec<u8>,
    display: String,
    addr: u64,
    function: Option<u64>,
) {
    // skip empty or duplicate key material
    if bytes.is_empty() || !seen.insert(bytes.clone()) {
        return;
    }
    keys.push(KeyCandidate {
        bytes,
        display,
        addr,
        function,
    });
}

/// Whether a candidate key and a ciphertext buffer have static affinity: one function
/// references both, or they are stored within [`KEY_REF_WINDOW`] bytes of each other.
///
/// The decrypt replay tries every key against every buffer, so the pairing itself
/// proves nothing. A key the program actually uses on a buffer is referenced by the
/// decrypting function, or sits right beside its ciphertext in one config/string
/// table; a pairing with neither tie is a brute-force coincidence and is demoted.
///
/// # Arguments
///
/// * `key` - The candidate key
/// * `ct` - The candidate ciphertext buffer
fn key_ct_affinity(key: &KeyCandidate, ct: &CtCandidate) -> bool {
    // a shared referencing function ties the pair directly (both must be known)
    if key.function.is_some() && key.function == ct.function {
        return true;
    }
    // adjacency: a key stored beside its ciphertext (one config/string table)
    key.addr != 0 && ct.addr != 0 && key.addr.abs_diff(ct.addr) <= KEY_REF_WINDOW
}

/// The severity and reasoning for one decrypt-replay recovery, demoting a pairing
/// with no key↔ciphertext affinity to the weak grade.
///
/// # Arguments
///
/// * `affine` - Whether the key and ciphertext passed [`key_ct_affinity`]
/// * `kind` - The classified kind of the recovered text
/// * `text` - The recovered text
/// * `scheme` - The replayed scheme's label
/// * `key_display` - The candidate key's display rendering
fn decrypt_severity(
    affine: bool,
    kind: RecoveredKind,
    text: &str,
    scheme: &str,
    key_display: &str,
) -> ((Confidence, i64), String) {
    // a keyed decryption of an affine pair is reliable (Likely base); content sets
    // suspicion and can only raise confidence further
    if affine {
        let assessment = decode::assess(high().0, kind, text);
        let reasoning = decode::append_assessment(
            decode::recovery_reasoning(CRYPTO_BASE_REASON, scheme, Some(key_display)),
            &assessment.notes,
        );
        ((assessment.confidence, assessment.suspicion), reasoning)
    } else {
        // with no affinity the replay alone carries the claim: the weak grade, and a
        // reasoning that names the missing tie so an agent knows what to check
        let base = decode::recovery_reasoning(CRYPTO_BASE_REASON, scheme, Some(key_display));
        let reasoning = format!(
            "{base}\n\nDemoted: no function references both the candidate key and this \
             buffer, and they sit more than {KEY_REF_WINDOW} bytes apart, so nothing in \
             the program ties the two together — the pairing exists only because the \
             replay tried every embedded string as a key against every buffer."
        );
        (low(), reasoning)
    }
}

/// Whether a recovery of this kind justifies attaching the decrypting key as a
/// [`ArtifactClass::SymmetricKey`] artifact.
///
/// Key-class recoveries validate the key that produced them; a plain recovered
/// string does not. The artifact classes are pivot-class — they exempt a hit from
/// runtime weakening and its thread from rollup suppression — so attaching the key
/// to a string-grade recovery would guarantee every fabrication a loud thread.
///
/// # Arguments
///
/// * `kind` - The classified kind of the recovered text
fn carries_key_artifact(kind: RecoveredKind) -> bool {
    matches!(
        kind,
        RecoveredKind::PublicKey
            | RecoveredKind::PrivateKey
            | RecoveredKind::Certificate
            | RecoveredKind::SymmetricKey
    )
}

/// The raw bytes of a base64-wrapped symmetric key, if `value` is pure base64 that
/// decodes to a standard AES/RC6 key length (16/24/32 bytes).
///
/// Gates on the base64 alphabet and a 4-aligned length so ordinary strings are not
/// spuriously "decoded", and on the decoded length so only real key sizes are surfaced;
/// a wrong candidate would in any case be rejected by the recovery guards downstream.
///
/// # Arguments
///
/// * `value` - The defined-string value to test
fn base64_key_bytes(value: &str) -> Option<Vec<u8>> {
    // a real base64 string is a multiple of 4 chars and uses only the base64 alphabet
    let len = value.len();
    if len < 4 || !len.is_multiple_of(4) || !value.bytes().all(is_base64_char) {
        return None;
    }
    let decoded = decode::base64::decode(value)?;
    // only surface a decode that is a standard symmetric key length
    matches!(decoded.len(), 16 | 24 | 32).then_some(decoded)
}

/// Gather candidate ciphertext buffers: mostly-non-printable data buffers the code
/// references, plus base64-unwrapped blobs.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if disassembling the program or reading its sections fails.
async fn collect_ciphertexts(ctx: &AnalysisContext) -> Result<Vec<CtCandidate>> {
    let sections = ctx.initialized_bytes().await?;
    let all = ctx.all_function_disassembly().await?;
    // the reverse index, used to attribute a base64-blob ciphertext (which comes from
    // a defined string, not a per-function scan) to its referencing function
    let index = ctx.reference_index().await?;

    let mut cts = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();

    // referenced data buffers (aligned starts, so RC4's keystream lines up). We are
    // iterating per function, so the referencing function is known directly.
    for (entry, insns) in &all {
        for addr in decode::buffers::data_addrs(insns) {
            if cts.len() >= MAX_CTS {
                break;
            }
            if !seen.insert(addr) {
                continue;
            }
            let Some((name, bytes)) = decode::buffers::slice_at(&sections, addr, MAX_CT_BYTES)
            else {
                continue;
            };
            // only real data sections hold ciphertext; skip pointer tables / linkage
            // sections (.got/.plt/relocations) whose zero bytes make a keyed XOR echo
            // the key back as a false "recovery"
            if !decode::buffers::is_data_section(name) {
                continue;
            }
            // a ciphertext is long and mostly non-printable
            if bytes.len() >= MIN_CT_BYTES && recover::src_nonprintable_ratio(bytes) >= 0.5 {
                cts.push(CtCandidate {
                    addr,
                    // this function references (and decrypts) the buffer
                    function: Some(*entry),
                    section: name.to_string(),
                    bytes: bytes.to_vec(),
                });
            }
        }
    }

    // base64-wrapped blobs: decode long base64 strings that unwrap to binary
    for string in &ctx.strings {
        if cts.len() >= MAX_CTS {
            break;
        }
        let value = string.value.trim();
        // a plausible base64 blob: long and all base64 alphabet
        if value.len() < 24 || !value.bytes().all(is_base64_char) {
            continue;
        }
        let Some(decoded) = decode::base64::decode(value) else {
            continue;
        };
        // keep it only if it unwraps to something non-printable (i.e. ciphertext)
        if decoded.len() >= MIN_CT_BYTES && recover::src_nonprintable_ratio(&decoded) >= 0.5 {
            let addr = crate::thorpal::detect::context::parse_addr(&string.address).unwrap_or(0);
            cts.push(CtCandidate {
                addr,
                // attribute the blob to the function that references its string
                function: decode::buffers::referencing_function(&index, addr, MAX_CT_BYTES as u64),
                section: "base64 string".to_string(),
                bytes: decoded,
            });
        }
    }
    Ok(cts)
}

/// Whether a byte is a base64 alphabet character (standard or URL-safe) or padding.
///
/// # Arguments
///
/// * `b` - The byte to classify
fn is_base64_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_' | b'=')
}

/// The base analyst explanation for a crypto-recovered item (the "how" is appended by
/// [`decode::recovery_reasoning`]).
const CRYPTO_BASE_REASON: &str = "Statically recovered by replaying a keyed cipher with an embedded candidate key over a \
     non-printable buffer — a statically-recovered encrypted payload/config. The key and plaintext \
     are candidates recovered by static analysis; confirm against the sample.";

/// The max distance (bytes) from a referenced data address at which a recovered PEM block
/// is still attributed to the referencing function.
const PEM_REF_WINDOW: u64 = 4096;

/// The max PEM key/certificate recoveries reported per run (mirrors [`MAX_DER_HITS`]).
///
/// A statically linked trust store or test suite embeds dozens to hundreds of armored
/// blocks, and itemizing them buries the sample's own key material.
const MAX_PEM_HITS: usize = 24;

/// The max certificates itemized per image before they collapse into one bundle lead.
///
/// A sample pinning its TLS peer or carrying operator material embeds a handful of
/// certificates; a statically linked trust store embeds dozens to hundreds.
const MAX_ITEMIZED_CERTS: usize = 3;

/// The max member addresses listed in the collapsed certificate-bundle lead's content
/// (mirrors the seeder's per-post site cap, so content and post agree on bounds).
const BUNDLE_LISTED: usize = 8;

/// The min stripped characters in one PEM body line for the block to count as holding
/// real key material.
///
/// The shortest real body in the wild — an Ed25519 `SubjectPublicKeyInfo` — is a
/// single 60-character line, so half that leaves margin while still rejecting a stray
/// word sitting between two PEM-writer string constants.
const MIN_PEM_BODY_LINE: usize = 32;

/// Known library-test and public trust-store subject markers, matched (lowercased)
/// against a PEM block's **decoded** body.
///
/// [`TEST_LABELS`] only sees armor labels, but a library's test material usually
/// marks itself in the certificate subject instead (`O=PolarSSL, CN=PolarSSL Test EC
/// CA`), and a bundled root CA names its operator there. Either way the block is the
/// linked library's baggage rather than the sample's own key, so a match demotes the
/// recovery with a caveat instead of letting it claim the top grade.
const LIBRARY_MATERIAL_SUBJECTS: &[&str] = &[
    // library test-suite material, marked in the subject/body rather than the armor
    "polarssl",
    "mbed tls",
    "testing key",
    // well-known public root-CA operators (trust-store bundle material)
    "digicert",
    "verisign",
    "globalsign",
    "baltimore cybertrust",
    "usertrust",
    "addtrust",
    "comodo",
    "sectigo",
    "entrust",
    "thawte",
    "geotrust",
    "go daddy",
    "starfield tech",
    "isrg root",
    "quovadis",
];

/// The max base64 characters looked back from a padding marker when carving an embedded
/// symmetric key (a ≤32-byte key encodes to ≤44 base64 characters). Bounds the scan.
const MAX_KEY_B64_LEN: usize = 44;

/// The minimum Shannon entropy (bits/byte) of a decoded blob to treat it as key material
/// (a random symmetric key is near the ~4-bit ceiling for 16 distinct bytes).
const MIN_KEY_ENTROPY: f32 = 3.5;

/// The max distance (bytes) from a referenced data address at which a carved key blob is
/// still attributed to the referencing function.
const KEY_REF_WINDOW: u64 = 4096;

/// The max gap (bytes) between neighbouring carved key tokens that still groups them
/// into one cluster.
///
/// Entries of one encoded string table sit back to back; 4KB is far past any table's
/// internal padding while keeping unrelated keys apart.
const KEY_CLUSTER_GAP: u64 = 4096;

/// The min carved tokens in one cluster to reshape the cluster into a single
/// string-table lead.
///
/// One or two key-length blobs are plausibly real embedded keys; three or more
/// same-shape blobs in one run is the shape of an encoded string table, where every
/// 16/24/32-char entry decodes to a "key-length high-entropy blob".
const KEY_CLUSTER_MIN: usize = 3;

/// The max carved tokens itemized image-wide before every cluster collapses.
///
/// A table whose entries sit far apart (or straddle sections) defeats the gap
/// grouping; past this many key-length carvings the image is storing encoded data in
/// bulk, and itemizing each blob as a separate candidate key is a flood, not keys.
const MAX_LONE_KEYS: usize = 4;

/// The max cluster member addresses listed in a collapsed lead's content (mirrors the
/// seeder's per-post site cap, so the content and the seeded post agree on bounds).
const KEY_CLUSTER_LISTED: usize = 8;

/// Recover and flag embedded PEM keys/certificates, surfacing the **full** block.
///
/// Two mechanisms feed one deduped result so the same key/cert is never reported twice:
///
/// * **(A) raw-byte scan** — [`recover::find_pem_blocks`] over every data-like section
///   finds a plaintext block the disassembler never defined as a string. This is the
///   common case in Go/packed binaries, where string data is one unterminated
///   `go.string.*` blob (this sample's RSA public key lives there).
/// * **(B) defined-string boundaries** — any [`PEM_STRINGS`] boundary in a defined string
///   is completed into its full block by reading the raw bytes at the string's address.
/// * **(C) encoded armor** — a long base64 run is decoded and the result re-scanned for
///   PEM blocks. A key stored behind one encoding layer is invisible to (A) and (B),
///   which both read the stored bytes literally, and "armored key stored encoded" is an
///   ordinary storage shape rather than anything family-specific.
///
/// A boundary that cannot be completed into a real key/cert (e.g. a runtime
/// `encoding/pem` template), or completes only into an internally-inconsistent block
/// (mismatched labels, no base64 body — a PEM-writer constant table), still emits the
/// boundary-present marker ([`pem_marker_hits`]), preserving the capability signal
/// without fabricating a recovery. Dedup is by recovered block **content**, shared
/// across all three mechanisms. The itemized recoveries are capped at
/// [`MAX_PEM_HITS`], and more than [`MAX_ITEMIZED_CERTS`] certificates collapse into
/// one bundle lead ([`certificate_bundle_hit`]) — a trust store must not bury the
/// sample's own key material.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `consumers` - The key-consuming functions, for artifact attribution
///
/// # Errors
///
/// Returns an error if reading the program's section bytes / references fails.
#[tracing::instrument(name = "crypto_capability::pem_hits", skip_all, err(Debug))]
async fn pem_hits(ctx: &AnalysisContext, consumers: &KeyConsumers) -> Result<Vec<DetectionHit>> {
    // read (cached) the section bytes and the data-reference index once
    let sections = ctx.initialized_bytes().await?;
    let index = ctx.reference_index().await?;
    // marker hits pass straight through; recovered blocks are gathered first so the
    // certificate-bundle collapse and the per-run cap can see the whole yield
    let mut hits = Vec::new();
    let mut recovered: Vec<(RecoveredKind, String, DetectionHit)> = Vec::new();
    // dedup key = recovered block text, shared across all three mechanisms
    let mut seen: HashSet<String> = HashSet::new();

    // mechanism A: scan the raw bytes of every data-like section for full PEM blocks
    for (name, base, bytes) in &sections {
        if !decode::buffers::is_data_section(name) {
            continue;
        }
        for (off, block) in recover::find_pem_blocks(bytes) {
            let addr = base.saturating_add(off as u64);
            if let Some((kind, hit)) =
                emit_pem(ctx, &index, consumers, &mut seen, addr, name, &block, None)
            {
                recovered.push((kind, name.clone(), hit));
            }
        }
    }

    // mechanism B: complete every PEM boundary found in a defined string into its block
    for string in &ctx.strings {
        // only strings that actually carry a PEM boundary are candidates
        let hay = string.value.to_ascii_lowercase();
        if !PEM_STRINGS.iter().any(|n| hay.contains(n)) {
            continue;
        }
        let Some(addr) = crate::thorpal::detect::context::parse_addr(&string.address) else {
            continue;
        };
        // read the raw bytes at the string's address and recover the block that starts
        // there (offset 0) — the body/closing boundary may run past the defined string
        let block_here = decode::slice_at(&sections, addr, recover::MAX_PEM_BLOCK).and_then(
            |(section, bytes)| {
                recover::find_pem_blocks(bytes)
                    .into_iter()
                    .find(|(off, _)| *off == 0)
                    .map(|(_, block)| (section, block))
            },
        );
        match block_here {
            // a full, internally-consistent block: emit typed content (dedup collapses
            // it against mechanism A)
            Some((section, block)) if is_valid_pem_block(&block) => {
                if let Some((kind, hit)) = emit_pem(
                    ctx, &index, consumers, &mut seen, addr, section, &block, None,
                ) {
                    recovered.push((kind, section.to_string(), hit));
                }
            }
            // a lone boundary, a mismatched BEGIN/END pair, or an armor pair with no
            // base64 body (a PEM-writer constant table): fall back to the
            // boundary-present marker, which claims the capability without fabricating
            // a recovery
            _ => hits.extend(pem_marker_hits(ctx, &index, string).await?),
        }
    }

    // mechanism C: decode long base64 runs and re-scan the output for armored keys
    for (name, base, bytes) in &sections {
        if !decode::buffers::is_data_section(name) {
            continue;
        }
        let (blobs, skipped) = find_base64_blobs(bytes);
        // a truncated scan is not a clean scan; say which section ran out of budget
        if skipped > 0 {
            tracing::warn!(
                section = name,
                scanned = blobs.len(),
                skipped,
                "encoded-armor scan truncated at the per-section decode budget"
            );
        }
        for (off, decoded) in blobs {
            let addr = base.saturating_add(off as u64);
            // the decoded bytes must still read as text before a PEM scan means anything
            let text = String::from_utf8_lossy(&decoded);
            for (_, block) in recover::find_pem_blocks(text.as_bytes()) {
                // the address is the *stored* (encoded) blob — that is where an analyst
                // has to look, and where the decoding function's reference points
                if let Some((kind, hit)) = emit_pem(
                    ctx,
                    &index,
                    consumers,
                    &mut seen,
                    addr,
                    name,
                    &block,
                    Some("base64"),
                ) {
                    recovered.push((kind, name.clone(), hit));
                }
            }
        }
    }

    // bundle a certificate flood and cap the itemized recoveries, then append the
    // shaped findings after the marker hits
    hits.extend(shape_pem_recoveries(recovered));
    Ok(hits)
}

/// Shape the gathered PEM recoveries for the report: collapse a certificate flood
/// into one bundle lead ([`certificate_bundle_hit`]) and cap the itemized remainder
/// at [`MAX_PEM_HITS`].
///
/// # Arguments
///
/// * `recovered` - Each recovered block's kind, section, and hit, in recovery order
fn shape_pem_recoveries(
    recovered: Vec<(RecoveredKind, String, DetectionHit)>,
) -> Vec<DetectionHit> {
    let mut hits = Vec::new();
    // collapse a certificate flood: more than MAX_ITEMIZED_CERTS distinct certificates
    // in one image is a trust-store/test bundle, not operator material
    let cert_count = recovered
        .iter()
        .filter(|(kind, _, _)| matches!(kind, RecoveredKind::Certificate))
        .count();
    let mut itemized: Vec<DetectionHit> = Vec::new();
    if cert_count > MAX_ITEMIZED_CERTS {
        // split the certificates into the bundle and keep everything else itemized
        let mut bundle: Vec<(String, u64)> = Vec::new();
        for (kind, section, hit) in recovered {
            if matches!(kind, RecoveredKind::Certificate) {
                bundle.push((section, hit.address));
            } else {
                itemized.push(hit);
            }
        }
        hits.push(certificate_bundle_hit(&bundle));
    } else {
        itemized.extend(recovered.into_iter().map(|(_, _, hit)| hit));
    }
    // cap the itemized recoveries the way MAX_DER_HITS caps DER structures, and say so
    let dropped = itemized.len().saturating_sub(MAX_PEM_HITS);
    itemized.truncate(MAX_PEM_HITS);
    if dropped > 0 {
        tracing::warn!(
            reported = itemized.len(),
            dropped,
            "PEM key/certificate recoveries truncated at the per-run cap"
        );
    }
    hits.extend(itemized);
    hits
}

/// Build the single collapsed lead for an image's embedded certificate bundle.
///
/// More than [`MAX_ITEMIZED_CERTS`] distinct certificates in one image is a trust
/// store or test bundle shipped by a statically linked TLS library, not operator
/// material, so the flood is reported as one context lead. No artifact is attached:
/// certificates are pivot-class (they veto runtime weakening and rollup
/// suppression), and a linked trust store has no claim to that shielding.
///
/// # Arguments
///
/// * `certs` - Each recovered certificate's `(section, address)`, in recovery order
fn certificate_bundle_hit(certs: &[(String, u64)]) -> DetectionHit {
    let n = certs.len();
    // anchor at the first recovered certificate
    let first = certs.first().map_or(0, |(_, addr)| *addr);
    // the sections the bundle spans, deduped for the metadata line
    let sections: BTreeSet<&str> = certs.iter().map(|(section, _)| section.as_str()).collect();
    let sections = sections.into_iter().collect::<Vec<_>>().join(", ");
    // list the first few member addresses; the full list rides on the hit's sites
    let listed: Vec<String> = certs
        .iter()
        .take(BUNDLE_LISTED)
        .map(|(_, addr)| crate::thorpal::detect::context::fmt_addr(*addr))
        .collect();
    let more = n.saturating_sub(BUNDLE_LISTED);
    let addr_list = match more {
        0 => listed.join(", "),
        more => format!("{} (+{more} more)", listed.join(", ")),
    };
    // the collapsed content: a count and the places to look, not N copies of a cert
    let content = format!(
        "### Embedded certificate bundle ({n} certs)\n\n### Metadata\n\
         - **Certificates:** {n}\n- **Sections:** {sections}\n- **Addresses:** {addr_list}\n"
    );
    let reasoning = format!(
        "The image embeds {n} distinct certificates. A sample pinning its own TLS peer or \
         carrying operator material embeds a handful; dozens is the shape of a CA trust \
         store or a library test bundle statically linked into the binary, so the bundle is \
         reported once rather than as {n} recovered-certificate findings. If one of these \
         matters it is the odd one out: look for a subject that is not a public root CA or \
         a library test CA."
    );
    DetectionHit::new(
        T1486,
        first,
        // a bundle spans functions; it is a fact about the image, not about one routine
        None,
        // context about the linked libraries, not a recovery claim
        medium(),
        content,
        reasoning,
    )
    .name(format!(
        "Certificate Bundle - {n} certificates @ 0x{first:x}"
    ))
    .kind(EvidenceKind::ContextOnly)
    .sites(certs.iter().map(|(_, addr)| *addr).collect())
}

/// The minimum characters in a base64 run for it to be worth decoding as possible
/// encoded armor. A PEM block's own armor lines alone exceed this, so a shorter run
/// cannot contain one and decoding it would only add noise.
const MIN_ARMOR_B64_LEN: usize = 96;

/// The maximum characters of a single base64 run decoded as possible encoded armor.
/// Bounds both the decode and the PEM re-scan on a section that is one long blob.
const MAX_ARMOR_B64_LEN: usize = 64 * 1024;

/// The max bytes decoded per section while looking for encoded armor.
///
/// A budget rather than a run count, deliberately: a statically linked binary's data
/// section is one long blob in which base64-alphabet runs are everywhere, so a
/// "first N runs" cap would spend itself on incidental text before reaching anything.
const MAX_ARMOR_DECODE_BYTES: usize = 4 * 1024 * 1024;

/// Carve long base64 runs out of raw section bytes and decode them.
///
/// The counterpart to [`find_base64_keys`], which deliberately looks only at short
/// padding-anchored tokens: this takes the opposite end of the range, where the
/// interesting case is a whole armored key that was encoded again before being
/// stored.
///
/// Whitespace inside the run is tolerated (an encoded PEM block is often stored
/// wrapped), but any other byte ends the run, so a decode never spans two unrelated
/// pieces of data.
///
/// Returns the `(offset, decoded)` pairs, plus the number of runs left unscanned once
/// the decode budget was spent — the caller reports that rather than letting the
/// truncation pass for full coverage.
///
/// # Arguments
///
/// * `bytes` - The raw section bytes to scan
fn find_base64_blobs(bytes: &[u8]) -> (Vec<(usize, Vec<u8>)>, usize) {
    let mut out = Vec::new();
    // bytes decoded so far, and runs skipped after the budget ran out
    let mut spent = 0usize;
    let mut skipped = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        // skip to the start of a base64-alphabet run
        if !is_base64_std(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        // grow the run over alphabet, padding, and embedded line breaks
        let mut chars = 0;
        while i < bytes.len() && chars < MAX_ARMOR_B64_LEN {
            let b = bytes[i];
            if is_base64_std(b) || b == b'=' {
                chars += 1;
                i += 1;
            } else if b == b'\n' || b == b'\r' {
                i += 1;
            } else {
                break;
            }
        }
        // only a run long enough to hold an armored block is worth decoding
        if chars < MIN_ARMOR_B64_LEN {
            continue;
        }
        // once the budget is spent, count the remaining runs instead of decoding them
        if spent >= MAX_ARMOR_DECODE_BYTES {
            skipped += 1;
            continue;
        }
        let run = String::from_utf8_lossy(&bytes[start..i]);
        if let Some(decoded) = decode::base64::decode(&run) {
            spent += decoded.len();
            out.push((start, decoded));
        }
    }
    (out, skipped)
}

/// Build one full-content flag for a recovered PEM block, or `None` when it is not a
/// real key/cert, is internally inconsistent, or is a duplicate. Returns the block's
/// classified kind alongside the hit so the caller can bundle certificates.
///
/// Classifies the block and only surfaces a public key, private key, or certificate (a
/// bare boundary/template classifies as a generic string and is skipped), requires an
/// internally-consistent block ([`is_valid_pem_block`]: matching BEGIN/END labels and
/// a decodable base64 body — PEM-writer constant tables fail both), and dedups by
/// content via `seen`. The flag carries the full block, a typed snippet-bearing name,
/// and a `Fact` confidence — a verbatim plaintext block has no decode uncertainty.
///
/// A block whose armor label, decoded body subject, or referencing function marks it
/// as library/test/trust-store material is weakened and annotated rather than dropped:
/// a standard library that statically links an HTTP or TLS stack ships its own test
/// key material into every binary built against it, and silently discarding the class
/// would also discard a real recovery whenever the sample's own key happens to sit in
/// the same company.
///
/// # Arguments
///
/// * `ctx` - The analysis context (resolves the referencing function's name)
/// * `index` - The sorted data-reference index (to attribute the block to a function)
/// * `consumers` - The key-consuming functions, for artifact attribution
/// * `seen` - The shared set of already-emitted block texts (dedup)
/// * `addr` - The absolute address the block starts at (the *stored* form's address)
/// * `section` - The section the block came from (e.g. `.rodata`)
/// * `block` - The recovered PEM block text
/// * `encoding` - The encoding the block was stored behind, if it was not stored verbatim
#[allow(clippy::too_many_arguments)]
fn emit_pem(
    ctx: &AnalysisContext,
    index: &[(u64, u64)],
    consumers: &KeyConsumers,
    seen: &mut HashSet<String>,
    addr: u64,
    section: &str,
    block: &str,
    encoding: Option<&str>,
) -> Option<(RecoveredKind, DetectionHit)> {
    // only a real key/cert armor is surfaced; a bare `-----BEGIN ` template is not
    let kind = decode::classify(block);
    if !matches!(
        kind,
        RecoveredKind::PublicKey | RecoveredKind::PrivateKey | RecoveredKind::Certificate
    ) {
        return None;
    }
    // require an internally-consistent block: a matching END label and a real base64
    // body (adjacent PEM-writer string constants weld into "blocks" with mismatched
    // labels or NUL/garbage bodies; a stored key never has either defect)
    if !is_valid_pem_block(block) {
        return None;
    }
    // dedup by content so no mechanism reports the same key/cert a second time
    if !seen.insert(block.to_owned()) {
        return None;
    }
    // attribute the block to the function that references it, when known
    let function = decode::buffers::referencing_function(index, addr, PEM_REF_WINDOW);
    // how the block was stored, which drives both the content's method line and the
    // wording of the reasoning below
    let stored = match encoding {
        Some(enc) => format!("{enc}-encoded (embedded PEM)"),
        None => "plaintext (embedded PEM)".to_string(),
    };
    // typed name + full-content markdown + independent confidence/suspicion assessment
    let name = decode::recovered_name(kind, block, section, addr);
    let content = decode::recovered_content(kind, block, section, addr, &stored, None);
    // a verbatim plaintext block has zero decode uncertainty → Fact; content sets
    // suspicion. A block recovered through a decode step is only as good as the decode,
    // so it starts at the keyed-recovery level instead
    let method = match encoding {
        Some(_) => high().0,
        None => Confidence::Fact,
    };
    let assessment = decode::assess(method, kind, block);
    // a dedicated reasoning (not `recovery_reasoning`, which would imply a decode step
    // for the verbatim case)
    let recovery = match encoding {
        Some(enc) => format!(
            "stored {enc}-encoded in the binary's {section} section and recovered by decoding \
             the run and locating both the `-----BEGIN` and `-----END` boundaries in the \
             decoded output — an extra encoding layer over key material is a storage choice \
             that hides it from a literal scan of the section bytes"
        ),
        None => format!(
            "stored verbatim (plaintext) in the binary's {section} section, recovered by \
             locating both the `-----BEGIN` and `-----END` boundaries in the raw section \
             bytes. The disassembler did not define it as a discrete string (common in \
             Go/packed binaries, where string data is one unterminated blob)"
        ),
    };
    let base = format!(
        "Found a PEM-armored {} {recovery}. An embedded RSA public key is the attacker key \
         ransomware uses to wrap each victim's file-encryption key; an embedded private key \
         or certificate is itself sensitive key material.",
        kind.label().to_ascii_lowercase(),
    );
    let reasoning = decode::append_assessment(base, &assessment.notes);
    let mut hit = DetectionHit::new(
        T1486,
        addr,
        function,
        (assessment.confidence, assessment.suspicion),
        content,
        reasoning.clone(),
    )
    .name(name)
    .kind(EvidenceKind::ContextOnly);
    // weaken (never drop) material the evidence marks as library or test key material
    let caveats = pem_caveats(ctx, block, function);
    if !caveats.is_empty() {
        hit.weaken();
        hit.reasoning = format!(
            "{reasoning}\n\nWeakened because {}. Reported rather than dropped: the block is \
             genuinely present, and suppressing the class would also hide a real key that \
             happens to sit alongside library material.",
            caveats.join(", and ")
        );
    }
    // tie the recovered key to the code that uses it (an unowned key is not triageable)
    let (role, detail) = key_attribution(consumers, function, "PEM block", PEM_REF_WINDOW);
    let mut artifact = HitArtifact::new(artifact_class(kind), pem_artifact_value(block));
    if let Some(role) = role {
        artifact = artifact.role(role);
    }
    // fold the encoding chain into the artifact detail alongside any attribution note
    let mut details: Vec<String> = detail.into_iter().collect();
    if let Some(enc) = encoding {
        details.push(format!("stored {enc}-encoded at this address"));
    }
    if !details.is_empty() {
        artifact = artifact.detail(details.join("; "));
    }
    Some((kind, hit.artifact(artifact)))
}

/// The caveats that mark a recovered PEM block as library/test material: a test
/// armor label, a known library/trust-store subject in the decoded body, or a
/// statically linked referencing function.
///
/// # Arguments
///
/// * `ctx` - The analysis context (resolves the referencing function's name)
/// * `block` - The recovered PEM block text
/// * `function` - The entry of the referencing function, when one was found
fn pem_caveats(ctx: &AnalysisContext, block: &str, function: Option<u64>) -> Vec<String> {
    let mut caveats = Vec::new();
    // a test-material armor label is the library marking the block itself
    if let Some(label) = test_label(block) {
        caveats.push(format!(
            "the PEM armor is labelled `{label}`, which standard libraries use to mark \
             deliberately non-production key material"
        ));
    }
    // a known test/trust-store subject inside the decoded body says the same thing
    if let Some(subject) = library_material_marker(block) {
        caveats.push(format!(
            "the decoded body carries the library/trust-store subject marker `{subject}` \
             (test-suite CAs and public root bundles ship inside statically linked TLS \
             libraries)"
        ));
    }
    // material referenced only by linked runtime code is the library's, not the sample's
    let runtime_name = function
        .and_then(|entry| ctx.function_name(entry))
        .filter(|name| is_library_function(name));
    if let Some(fn_name) = runtime_name {
        caveats.push(format!(
            "the referencing function `{fn_name}` is statically linked runtime code \
             rather than the program's own"
        ));
    }
    caveats
}

/// The library-test or trust-store subject marker a PEM block's **decoded** body
/// carries, if any.
///
/// Subjects live inside the DER, not in the armor, so the base64 body is decoded and
/// searched (lowercased) for the [`LIBRARY_MATERIAL_SUBJECTS`] — DER stores subject
/// names as printable ASCII, so a lossy conversion finds them byte-exactly.
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
fn library_material_marker(block: &str) -> Option<&'static str> {
    // gather and strip the base64 body between the boundaries
    let body: String = block
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .flat_map(str::chars)
        .filter(|c| !c.is_whitespace())
        .collect();
    // decode to the DER bytes the subject names actually live in
    let decoded = decode::base64::decode(&body)?;
    let hay = String::from_utf8_lossy(&decoded).to_ascii_lowercase();
    LIBRARY_MATERIAL_SUBJECTS
        .iter()
        .copied()
        .find(|subject| hay.contains(subject))
}

/// The artifact class a recovered-key kind maps to.
///
/// Kinds that are not key material never reach here — the callers all gate on the
/// key/certificate kinds first — so an unexpected kind is recorded as generic decoded
/// data rather than silently mislabelled as a key.
///
/// # Arguments
///
/// * `kind` - The classified kind of the recovery
fn artifact_class(kind: RecoveredKind) -> ArtifactClass {
    match kind {
        RecoveredKind::PrivateKey => ArtifactClass::AsymmetricPrivateKey,
        RecoveredKind::PublicKey => ArtifactClass::AsymmetricPublicKey,
        RecoveredKind::Certificate => ArtifactClass::Certificate,
        RecoveredKind::SymmetricKey => ArtifactClass::SymmetricKey,
        RecoveredKind::String | RecoveredKind::Email | RecoveredKind::Url => {
            ArtifactClass::DecodedData
        }
    }
}

/// The artifact value for a PEM block: its armor label plus the first body line, which
/// is enough for an analyst to recognize and pivot on without pasting a whole key into
/// a table cell (the full block is in the finding's content).
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
fn pem_artifact_value(block: &str) -> String {
    // the opening boundary names what the block is
    let label = block
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("-----BEGIN"))
        .unwrap_or("");
    // the first body line distinguishes two blocks that share a label
    let body = block
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("-----"))
        .unwrap_or("");
    format!("{label} {}", short_value(body))
}

/// The [`TEST_LABELS`] phrase a PEM block's armor carries, if any.
///
/// Word-bounded, because these phrases sit inside longer words often enough to
/// matter — `latest keyring` contains `test key` — and mislabelling a real key as
/// library test material is exactly the error that loses it.
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
fn test_label(block: &str) -> Option<&'static str> {
    // only the armor labels are inspected; a phrase inside the base64 body is noise.
    // The `-----` delimiters are stripped first because they read as part of a token
    // to the boundary check, which would reject every real armor line
    let armor: String = block
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("-----"))
        .map(|line| line.trim_matches('-').trim())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    // a phrase counts only when it stands as its own words in the label
    TEST_LABELS.iter().copied().find(|phrase| {
        armor.match_indices(phrase).any(|(at, _)| {
            crate::thorpal::detect::indicators::word_bounded(&armor, at, phrase.len())
        })
    })
}

/// The max distance (bytes) from a referenced data address at which a recovered DER
/// structure is still attributed to the referencing function.
const DER_REF_WINDOW: u64 = 4096;

/// The max DER structures reported. A binary that embeds a certificate bundle holds
/// hundreds of them, and listing every root CA would bury the sample's own key.
const MAX_DER_HITS: usize = 24;

/// The PEM body width, in base64 characters per line.
const PEM_LINE_WIDTH: usize = 64;

/// Recover **unarmored** key material: DER/ASN.1 structures stored as raw bytes.
///
/// [`pem_hits`] can only find key material that announces itself with a text boundary.
/// A key stored in its binary encoding announces nothing, so it is found instead by
/// validating structure: [`der::scan`] accepts a region only when it parses as a
/// complete, exactly-tiling ASN.1 object of a recognized key or certificate shape.
/// That completeness requirement is the whole false-positive control — the two-byte
/// DER header alone collides with ordinary binary data constantly.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `consumers` - The key-consuming functions, for artifact attribution
///
/// # Errors
///
/// Returns an error if reading the program's section bytes / references fails.
#[tracing::instrument(name = "crypto_capability::der_hits", skip_all, err(Debug))]
async fn der_hits(ctx: &AnalysisContext, consumers: &KeyConsumers) -> Result<Vec<DetectionHit>> {
    // read (cached) the section bytes and the data-reference index once
    let sections = ctx.initialized_bytes().await?;
    let index = ctx.reference_index().await?;
    let mut hits = Vec::new();
    // dedup by the raw structure bytes, so the same object found twice reports once
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    // how many structures were found beyond the cap, so a truncation is never silent
    let mut dropped = 0usize;
    for (name, base, bytes) in &sections {
        // only real data sections hold stored key material
        if !decode::buffers::is_data_section(name) {
            continue;
        }
        for found in der::scan(bytes) {
            let Some(raw) = bytes.get(found.at..found.at.saturating_add(found.len)) else {
                continue;
            };
            if !seen.insert(raw.to_vec()) {
                continue;
            }
            // count past the cap instead of emitting, and report the total below
            if hits.len() >= MAX_DER_HITS {
                dropped += 1;
                continue;
            }
            let addr = base.saturating_add(found.at as u64);
            hits.push(emit_der(
                ctx, &index, consumers, addr, name, found.kind, raw,
            ));
        }
    }
    // a silent cap reads as "this is everything", which for key material is misleading
    if dropped > 0 {
        tracing::warn!(
            reported = hits.len(),
            dropped,
            "DER key/certificate structures truncated at the per-run cap"
        );
    }
    Ok(hits)
}

/// Build the finding for one recovered DER structure.
///
/// # Arguments
///
/// * `ctx` - The analysis context (resolves the referencing function's name)
/// * `index` - The sorted data-reference index (to attribute the structure)
/// * `consumers` - The key-consuming functions, for artifact attribution
/// * `addr` - The absolute address the structure starts at
/// * `section` - The section the structure came from (e.g. `.rdata`)
/// * `kind` - The ASN.1 shape the validator recognized
/// * `raw` - The structure's raw bytes
fn emit_der(
    ctx: &AnalysisContext,
    index: &[(u64, u64)],
    consumers: &KeyConsumers,
    addr: u64,
    section: &str,
    kind: der::DerKind,
    raw: &[u8],
) -> DetectionHit {
    // render the structure as PEM-style armor so the analyst can copy it straight out
    let armored = armor_der(kind, raw);
    let recovered = kind.recovered_kind();
    let name = decode::recovered_name(recovered, &armored, section, addr);
    let content = der_content(kind, raw, section, addr, &armored);
    // a structural validator is strong but not verbatim text: it accepts on shape, so
    // the method confidence is the keyed-recovery level rather than `Fact`
    let assessment = decode::assess(high().0, recovered, &armored);
    let base = format!(
        "Found a {} stored as raw DER/ASN.1 bytes (no PEM armor) in the binary's {section} \
         section at 0x{addr:x}. Recovered by walking the ASN.1 length fields: the region was \
         accepted only because it parses as one complete object whose declared lengths tile \
         its children exactly at every level and whose inner shape matches a {} structure — \
         arbitrary binary data does not satisfy that. {}",
        kind.label(),
        kind.label(),
        if kind.is_private() {
            "A private key stored in the image is sensitive material in its own right and \
             lets an analyst decrypt whatever it protects."
        } else {
            "An embedded public key or certificate is the counterpart the program pins or \
             wraps data to, and identifies the operator's key rather than the victim's."
        },
    );
    let reasoning = decode::append_assessment(base, &assessment.notes);
    // attribute the structure to the function that references it, when known
    let function = decode::buffers::referencing_function(index, addr, DER_REF_WINDOW);
    let mut hit = DetectionHit::new(
        T1486,
        addr,
        function,
        (assessment.confidence, assessment.suspicion),
        content,
        reasoning.clone(),
    )
    .name(name)
    .kind(EvidenceKind::ContextOnly);

    // library-shipped key material is weakened and annotated, never dropped
    if let Some(fn_name) = function.and_then(|entry| ctx.function_name(entry)) {
        if is_library_function(fn_name) {
            hit.weaken();
            hit.reasoning = format!(
                "{reasoning}\n\nWeakened because the referencing function `{fn_name}` is \
                 statically linked runtime code rather than the program's own. Reported \
                 rather than dropped: the structure is genuinely present in the image."
            );
        }
    }

    // tie the recovered key to the code that uses it
    let (role, detail) = key_attribution(consumers, function, "DER structure", DER_REF_WINDOW);
    let mut artifact = HitArtifact::new(
        artifact_class(recovered),
        format!(
            "{} ({} bytes DER) @ {}",
            kind.label(),
            raw.len(),
            crate::thorpal::detect::context::fmt_addr(addr)
        ),
    );
    if let Some(role) = role {
        artifact = artifact.role(role);
    }
    if let Some(detail) = detail {
        artifact = artifact.detail(detail);
    }
    hit.artifact(artifact)
}

/// Render a DER structure as PEM-style armor, so a binary-stored key can be copied out
/// of the report and fed to an ordinary tool.
///
/// The armor label is the one the structure's own shape implies; this is a rendering of
/// what was found, not a claim that the sample stored it armored (the finding's
/// reasoning states that it did not).
///
/// # Arguments
///
/// * `kind` - The ASN.1 shape the validator recognized
/// * `raw` - The structure's raw bytes
fn armor_der(kind: der::DerKind, raw: &[u8]) -> String {
    // the standard armor label for each recognized shape
    let label = match kind {
        der::DerKind::Certificate => "CERTIFICATE",
        der::DerKind::SubjectPublicKeyInfo => "PUBLIC KEY",
        der::DerKind::RsaPublicKey => "RSA PUBLIC KEY",
        der::DerKind::RsaPrivateKey => "RSA PRIVATE KEY",
        der::DerKind::Pkcs8PrivateKey => "PRIVATE KEY",
        der::DerKind::EcPrivateKey => "EC PRIVATE KEY",
    };
    let body = decode::base64::wrap(&decode::base64::encode(raw), PEM_LINE_WIDTH);
    format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----")
}

/// Build the flag `content` markdown for a recovered DER structure: the armored
/// rendering in a fenced block plus a metadata list.
///
/// # Arguments
///
/// * `kind` - The ASN.1 shape the validator recognized
/// * `raw` - The structure's raw bytes
/// * `section` - The section the structure came from
/// * `addr` - The absolute address the structure starts at
/// * `armored` - The PEM-style rendering from [`armor_der`]
fn der_content(kind: der::DerKind, raw: &[u8], section: &str, addr: u64, armored: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("### Recovered {}\n\n", kind.label()));
    out.push_str("```\n");
    out.push_str(armored);
    out.push_str("\n```\n");
    out.push_str("\n### Metadata\n");
    out.push_str(&format!("- **Section:** {section}\n"));
    out.push_str(&format!("- **Address:** 0x{addr:x}\n"));
    out.push_str(&format!(
        "- **Stored as:** raw DER/ASN.1, {} bytes (no armor)\n",
        raw.len()
    ));
    out.push_str(&format!("- **Structure:** {}\n", kind.label()));
    out
}

/// Whether a recovered PEM block is internally consistent: its `-----END` label
/// matches its `-----BEGIN` label and a real base64 body sits between them.
///
/// mbedTLS/OpenSSL PEM-writer constant tables store `-----BEGIN X-----` and
/// `-----END Y-----` strings back to back, and a boundary-pair scan welds them into
/// "blocks" with mismatched labels and NUL/garbage "bodies". A real stored key or
/// certificate never has either defect, so both checks together cost nothing real.
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
fn is_valid_pem_block(block: &str) -> bool {
    pem_labels_match(block) && has_pem_body(block)
}

/// Whether a PEM block's `-----END` label equals its `-----BEGIN` label.
///
/// A block whose armor cannot supply both labels (a truncated `-----END` with no
/// closing dash run) fails too: every real armor closes both boundaries.
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
fn pem_labels_match(block: &str) -> bool {
    // both labels must parse and agree for the pair to be one block's armor
    match (pem_label(block, "-----begin"), pem_label(block, "-----end")) {
        (Some(begin), Some(end)) => begin == end,
        _ => false,
    }
}

/// The lowercased armor label following `marker` in a PEM block, when its closing
/// dash run is present (`-----BEGIN RSA PRIVATE KEY-----` → `rsa private key`).
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
/// * `marker` - The lowercase boundary marker (`"-----begin"` or `"-----end"`)
fn pem_label(block: &str, marker: &str) -> Option<String> {
    // search lowercased so armor case never matters (byte offsets are preserved)
    let lower = block.to_ascii_lowercase();
    let at = lower.find(marker)?;
    let rest = &lower[at + marker.len()..];
    // the label runs to the closing dash run; a truncated armor has no label
    let close = rest.find("-----")?;
    Some(rest[..close].trim().to_string())
}

/// Whether a PEM block carries a real base64 body between its boundaries: every body
/// line is base64 alphabet/padding/whitespace only, and at least one line is long
/// enough ([`MIN_PEM_BODY_LINE`]) to be key material and decodes.
///
/// This is what separates a stored key from a PEM-writer constant table: the NUL and
/// garbage bytes between two adjacent writer string constants read as non-empty,
/// non-boundary "body" lines (`str::trim` strips only whitespace), but never as
/// decodable base64. An armor-only `BEGIN`/`END` pair has no body line at all and is
/// rejected the same way.
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
fn has_pem_body(block: &str) -> bool {
    // whether any body line was long enough to be key material and decoded
    let mut decodes = false;
    for line in block.lines().map(str::trim) {
        // boundaries and blank lines are not body
        if line.is_empty() || line.starts_with("-----") {
            continue;
        }
        // a body line may hold only base64 alphabet, padding, and whitespace
        if !line
            .bytes()
            .all(|b| is_base64_std(b) || b == b'=' || b.is_ascii_whitespace())
        {
            return false;
        }
        // count a full-length line that decodes as the real-key-material evidence
        let stripped: String = line
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '=')
            .collect();
        if stripped.len() >= MIN_PEM_BODY_LINE && decode::base64::decode(&stripped).is_some() {
            decodes = true;
        }
    }
    decodes
}

/// Emit the boundary-present marker hit(s) for a defined string whose PEM boundary could
/// not be completed into a full block (a runtime template, not a baked-in key).
///
/// Mirrors [`crate::thorpal::detect::detections::string_artifact_hits`] for this one string: a single hit
/// naming every code cross-reference to it, or a single low-confidence hit at the string
/// itself when it is unreferenced. An unreferenced boundary is still attributed to the
/// function that references its buffer (via the reference index) when one does, so the
/// flag links there rather than the sample.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `index` - The sorted data-reference index (to attribute an unreferenced boundary)
/// * `string` - The defined string carrying the PEM boundary
///
/// # Errors
///
/// Returns an error if the bridge xref query fails.
async fn pem_marker_hits(
    ctx: &AnalysisContext,
    index: &[(u64, u64)],
    string: &ghidra_cli::models::StringData,
) -> Result<Vec<DetectionHit>> {
    use anyhow::Context as _;
    let mut hits = Vec::new();
    // a short, quote-safe rendering of the boundary string for the flag content
    let shown = short_value(&string.value);
    // resolve the code cross-references to this string
    let xrefs = ctx.xrefs_to(&string.address).await.with_context(|| {
        format!(
            "resolving references to PEM boundary string at {}",
            string.address
        )
    })?;
    if xrefs.is_empty() {
        // unreferenced boundary: a weaker signal, anchored at the string address, but
        // attributed to the function that references its buffer when one does
        let at = crate::thorpal::detect::context::parse_addr(&string.address).unwrap_or(0);
        let function = ctx
            .containing_function(at)
            .or_else(|| decode::buffers::referencing_function(index, at, PEM_REF_WINDOW));
        hits.push(
            DetectionHit::new(
                T1486,
                at,
                function,
                low(),
                format!("string {shown:?}"),
                format!("{PEM_MARKER_REASON} (string not referenced by code in this analysis)"),
            )
            .kind(EvidenceKind::ContextOnly),
        );
        return Ok(hits);
    }
    // one hit for the boundary string, naming every site it is referenced from: the
    // same address moved through registers is one use, not one finding per instruction
    let sites = crate::thorpal::detect::detections::xref_sites(&xrefs);
    if let Some(hit) = crate::thorpal::detect::detections::referenced_literal_hit(
        T1486,
        &shown,
        sites,
        high(),
        PEM_MARKER_REASON,
        |site| ctx.containing_function(site),
    ) {
        hits.push(hit);
    }
    Ok(hits)
}

/// A short (≤80 char) rendering of a string value for a marker flag's content.
///
/// # Arguments
///
/// * `s` - The string value to shorten
fn short_value(s: &str) -> String {
    const MAX: usize = 80;
    // return as-is when short enough, else clip with an ellipsis
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX).collect();
        format!("{head}...")
    }
}

/// Extract the readable recovered runs from a candidate decryption, applying the
/// key-echo guard and a stricter text gate for keyed XOR, plus the strong-recovery
/// gate.
///
/// Returns `None` when nothing survives (a wrong key). The `key_guard`, when set
/// (repeating XOR), rejects runs whose source region is mostly zero or that share a
/// long substring with the key — both signs of the key merely echoing through
/// low-entropy bytes rather than a real decryption — and additionally holds each run
/// to [`xor_run_is_wordy`], because a wrong XOR key over semi-structured data yields
/// *printable* output that clears the shared wordiness bar about half the time. The
/// caller de-duplicates and splits the surviving runs into items.
///
/// # Arguments
///
/// * `plain` - The candidate decrypted bytes
/// * `ct` - The ciphertext bytes (index-aligned with `plain`)
/// * `key_guard` - The key bytes to guard against echoing, for keyed XOR
fn recovered_runs(
    plain: &[u8],
    ct: &[u8],
    key_guard: Option<&[u8]>,
) -> Option<Vec<(usize, String)>> {
    // require genuinely-hidden readable runs (source non-printable + decoded wordy)
    let runs = recover::hidden_runs(plain, ct);
    // apply the key-echo guard (keyed XOR), keeping each surviving run's offset
    let mut kept: Vec<(usize, String)> = Vec::new();
    for (idx, s) in runs {
        if let Some(key) = key_guard {
            // reject key-echo: a region that does not look like ciphertext decodes to
            // the key rather than to anything the key was hiding
            let end = (idx + s.len()).min(ct.len());
            let region = &ct[idx..end];
            if is_degenerate_source(region) {
                continue;
            }
            // reject text that shares a long run with the key (the key bleeding through)
            if shares_substring(&s, &String::from_utf8_lossy(key), 5) {
                continue;
            }
            // reject output that is printable but not text: wrong-XOR fabrications
            // sit near the half-letters floor the shared wordiness bar accepts
            if !xor_run_is_wordy(s.as_bytes()) {
                continue;
            }
        }
        kept.push((idx, s));
    }
    // require a real recovery: substantial text that dominates a contiguous region.
    // This rejects both the coincidental short runs a brute force over many keys hits
    // and a keyed-XOR key-echo scattered through otherwise-random bytes.
    if !recover::is_strong_recovery(&kept, recover::MIN_RECOVERY_CHARS) {
        return None;
    }
    Some(kept)
}

/// One candidate decryption: the scheme's name, the bytes it produced, and the key
/// to test that output against for key-echo (`None` when the scheme cannot echo one).
type SchemeAttempt<'a> = (&'a str, Vec<u8>, Option<&'a [u8]>);

/// Whether a repeating-XOR recovery reads as genuine text: at least two thirds ASCII
/// letters, with at least one vowel-bearing word — or digit-substituted prose that
/// the shared leet-aware gate accepts.
///
/// The shared [`recover::is_wordy`] bar (half letters, five distinct characters) is
/// calibrated for ciphers whose wrong keys produce random bytes. A wrong repeating-XOR
/// key over semi-structured library data produces *printable* output, and about half
/// of the printable range is letters, so keyboard-mash fabrications like
/// `$&'{b-ANjegQ+` clear that bar. Real hidden strings — URLs, paths, messages — run
/// well above two-thirds letters and always contain a pronounceable word; the
/// fabrications have neither. Leetspeak ransom notes ("A44EN4ION!") carry digits
/// inside their words and fail both local tests, so the shared gate's prose-shaped
/// digit-substitution second chance is consulted before rejecting.
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
fn xor_run_is_wordy(run: &[u8]) -> bool {
    // test at full strength first, then once more counting leet digits as letters
    wordy_at_crypto_bar(run, false) || wordy_at_crypto_bar(run, true)
}

/// The crypto-strength wordiness test, optionally counting leet digits as letters.
///
/// The leet pass keeps the same two-thirds share and vowel-word bars but maps the
/// digit substitutions ransom notes use (`A44EN4ION!`, `IN O2DE2`) onto letters —
/// 0→o, 1→i, 3→e, 4→a, plus 2/5/7/9 as consonants — and only for prose-shaped runs
/// (a space and a real-letter third), so printable keyboard mash from wrong
/// repeating-XOR keys stays dead under both passes.
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
/// * `leet` - Whether to count digit substitutions as letters
fn wordy_at_crypto_bar(run: &[u8], leet: bool) -> bool {
    // the leet pass only applies to prose-shaped runs: a space present and at
    // least a third real letters, so digit soup cannot buy its way in
    if leet {
        let real = run.iter().filter(|b| b.is_ascii_alphabetic()).count();
        if !run.contains(&b' ') || real * 3 < run.len() {
            return false;
        }
    }
    // classify a byte as letter-like under the requested pass
    let letterish = |b: u8| {
        b.is_ascii_alphabetic()
            || (leet && matches!(b, b'0' | b'1' | b'2' | b'3' | b'4' | b'5' | b'7' | b'9'))
    };
    // require a letter share of at least 2/3 (wrong-key printable output sits near 1/2)
    let alpha = run.iter().filter(|b| letterish(**b)).count();
    if alpha * 3 < run.len() * 2 {
        return false;
    }
    // require at least one word (maximal letter-like run) carrying a vowel; the
    // leet pass lets the vowel-shaped digits 0/1/3/4 satisfy it
    run.split(|b| !letterish(*b)).any(|word| {
        !word.is_empty()
            && word.iter().any(|b| {
                matches!(b.to_ascii_lowercase(), b'a' | b'e' | b'i' | b'o' | b'u')
                    || (leet && matches!(b, b'0' | b'1' | b'3' | b'4'))
            })
    })
}

/// Whether a region claimed to be ciphertext is dominated by one repeated byte
/// value, and so cannot be ciphertext.
///
/// Encrypted and compressed bytes are close to uniform: any given value shows up
/// about once every 256 bytes. A sparse lookup table, a run of padding, or a block
/// of zeroes is the opposite, and XORing one with a printable key hands back the
/// key stream itself — printable, readable-looking, and completely fabricated. The
/// substring guard alone does not catch it, because the echo is broken up wherever
/// the source is *not* the dominant byte, so no single window matches the key.
///
/// The bar is a count rather than a pure ratio so a short run is not condemned by
/// the birthday problem: four repeats of one value inside sixteen uniform bytes is
/// already a one-in-many-thousands event, while two is unremarkable.
///
/// A histogram is not the only way a region can fail to be uniform, and the other
/// ways are the ones that cost the most: a table whose bytes *count*, and a table of
/// integers stored wider than the values in it. Index tables, SIMD shuffle masks,
/// relocation slot numbers and jump-table offsets spread their bytes evenly enough
/// over a dozen-odd distinct values that no single histogram bar stands out — while
/// remaining as far from random as bytes can get. Both shapes are
/// [`recover::is_structured_source`], which every decode consumer shares.
///
/// Note that the *breadth* of the source distribution is deliberately not tested.
/// A region of mostly control bytes looks nothing like ciphertext, but it is
/// exactly what printable plaintext under a printable key produces, so rejecting
/// it would throw out the most common genuine case this detection exists to find.
/// The same goes for the region's numeric range: lowercase plaintext under a
/// lowercase key lands every byte inside a window barely 32 wide, which is no
/// broader than the tables this rejects.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
fn is_degenerate_source(region: &[u8]) -> bool {
    // an empty region proves nothing was decrypted
    if region.is_empty() {
        return true;
    }
    // tally the region's byte histogram
    let mut counts = [0usize; 256];
    for byte in region {
        counts[*byte as usize] += 1;
    }
    // one value carrying an eighth of the region is not a uniform distribution
    let limit = std::cmp::max(4, region.len() / 8);
    if counts.iter().any(|count| *count >= limit) {
        return true;
    }
    // ...nor is a region that is laid out as a table rather than encrypted
    recover::is_structured_source(region)
}

/// Whether `a` and `b` share any common substring of at least `n` bytes.
///
/// # Arguments
///
/// * `a` - The first string
/// * `b` - The second string
/// * `n` - The minimum shared-substring length
fn shares_substring(a: &str, b: &str, n: usize) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() < n || b.len() < n {
        return false;
    }
    // slide an n-byte window over `a` and look for it anywhere in `b`
    a.windows(n).any(|win| b.windows(n).any(|w| w == win))
}

/// One carved candidate key token, kept with its location so neighbouring tokens can
/// be clustered and attributed.
struct CarvedKey {
    /// The section the token was carved from.
    section: String,
    /// The absolute address the token starts at.
    addr: u64,
    /// The base64 token as stored in the binary.
    token: String,
    /// The decoded raw key bytes.
    decoded: Vec<u8>,
}

/// Recover and flag embedded **symmetric key** material (ATT&CK T1486).
///
/// Malware bakes a symmetric key (RC6/AES/…) into the binary as a base64 blob, often
/// inside one concatenated string blob the disassembler never splits into a discrete
/// string (the Go/packed case — so [`collect_keys`], which reads defined strings, misses
/// it). This scans each data-like section's raw bytes for a padded base64 token that
/// decodes to a standard key length (16/24/32 bytes) of high-entropy bytes — a candidate
/// embedded key. It mirrors [`pem_hits`]'s raw-byte recovery of *asymmetric* key
/// material.
///
/// A lone carving (or a pair) is flagged at full severity as a candidate key. A run of
/// same-shape tokens is not N keys: every entry of a base64-wrapped encrypted string
/// table also decodes to a key-length high-entropy blob, so clusters
/// ([`cluster_collapses`]) are reshaped into **one** correctly-framed string-table lead
/// ([`key_cluster_hit`]) — which also caps what a table-heavy image can emit.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `consumers` - The key-consuming functions, for artifact attribution
///
/// # Errors
///
/// Returns an error if reading the program's section bytes / references fails.
#[tracing::instrument(name = "crypto_capability::embedded_key_hits", skip_all, err(Debug))]
async fn embedded_key_hits(
    ctx: &AnalysisContext,
    consumers: &KeyConsumers,
) -> Result<Vec<DetectionHit>> {
    // read (cached) the section bytes and the data-reference index once
    let sections = ctx.initialized_bytes().await?;
    let index = ctx.reference_index().await?;
    // carve every candidate token first (deduped by decoded key bytes), because the
    // itemize-or-collapse decision depends on how many the whole image holds
    let mut carved: Vec<CarvedKey> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (section, base, bytes) in &sections {
        // only real data sections hold embedded key material
        if !decode::buffers::is_data_section(section) {
            continue;
        }
        for (off, token, decoded) in find_base64_keys(bytes) {
            // one entry per distinct key
            if !seen.insert(decoded.clone()) {
                continue;
            }
            carved.push(CarvedKey {
                section: section.clone(),
                addr: base.saturating_add(off as u64),
                token,
                decoded,
            });
        }
    }
    // group neighbouring tokens into per-section clusters and shape each one once
    let total = carved.len();
    let mut hits = Vec::new();
    for cluster in cluster_carved_keys(carved) {
        if cluster_collapses(cluster.len(), total) {
            // same-shape key-length blobs in one run are an encoded string table, not
            // independent keys: emit the one correctly-framed table lead
            let function = cluster.iter().find_map(|key| {
                decode::buffers::referencing_function(&index, key.addr, KEY_REF_WINDOW)
            });
            let mut hit = key_cluster_hit(&cluster, total, function);
            // library-shipped material is weakened and annotated, never dropped
            let runtime_name = function
                .and_then(|entry| ctx.function_name(entry))
                .filter(|name| is_library_function(name));
            if let Some(fn_name) = runtime_name {
                hit.weaken();
                hit.reasoning = format!(
                    "{} Weakened because the referencing function `{fn_name}` is statically \
                     linked runtime code rather than the program's own.",
                    hit.reasoning
                );
            }
            hits.push(hit);
        } else {
            // a lone (or paired) carving is plausibly a real embedded key: itemize it
            // at full severity, as a single baked-in RC6/AES key deserves
            for key in cluster {
                hits.push(embedded_key_hit(ctx, &index, consumers, &key));
            }
        }
    }
    Ok(hits)
}

/// Build the full-severity finding for one itemized carved candidate key.
///
/// # Arguments
///
/// * `ctx` - The analysis context (resolves the referencing function's name)
/// * `index` - The sorted data-reference index (to attribute the key blob)
/// * `consumers` - The key-consuming functions, for artifact attribution
/// * `key` - The carved token to flag
fn embedded_key_hit(
    ctx: &AnalysisContext,
    index: &[(u64, u64)],
    consumers: &KeyConsumers,
    key: &CarvedKey,
) -> DetectionHit {
    let CarvedKey {
        section,
        addr,
        token,
        decoded,
    } = key;
    // attribute the key to the function that references its buffer, when known
    let function = decode::buffers::referencing_function(index, *addr, KEY_REF_WINDOW);
    let bits = decoded.len() * 8;
    // typed, snippet-bearing name; full-content markdown; analyst reasoning
    let name = format!(
        "Symmetric Key (candidate) - \"{}\" @ {section} @ 0x{addr:x}",
        short_value(token)
    );
    let content = key_content(token, section, *addr, decoded);
    let base = format!(
        "Found an embedded base64-encoded {}-byte ({bits}-bit) high-entropy blob in the \
         {section} section — a candidate symmetric encryption key (e.g. RC6/AES). Recovered \
         by carving a padded base64 token from the raw section bytes and decoding it; the \
         disassembler did not define it as a discrete string (common in Go/packed binaries, \
         where string data is one concatenated blob). Malware embeds such a key to encrypt \
         payloads/config or victim files (Data Encrypted for Impact). The key is a static \
         candidate; confirm against the sample.",
        decoded.len(),
    );
    // scored through the shared recovery assessment so a symmetric key is ranked
    // on the same axes as every other recovery. Raw key bytes have no
    // self-validating structure, so the carving method's own reliability is the
    // confidence, and the class sets the suspicion
    let assessment = decode::assess(high().0, RecoveredKind::SymmetricKey, token);
    let reasoning = decode::append_assessment(base, &assessment.notes);
    let mut hit = DetectionHit::new(
        T1486,
        *addr,
        function,
        (assessment.confidence, assessment.suspicion),
        content,
        reasoning,
    )
    .name(name)
    .kind(EvidenceKind::ContextOnly);
    // library-shipped material is weakened and annotated, never dropped
    let runtime_name = function
        .and_then(|entry| ctx.function_name(entry))
        .filter(|name| is_library_function(name));
    if let Some(fn_name) = runtime_name {
        hit.weaken();
        hit.reasoning = format!(
            "{} Weakened because the referencing function `{fn_name}` is statically \
             linked runtime code rather than the program's own.",
            hit.reasoning
        );
    }
    // tie the key to the code that uses it, naming what that code was seen doing
    let (role, detail) = key_attribution(consumers, function, "key blob", KEY_REF_WINDOW);
    let hex: String = decoded.iter().map(|b| format!("{b:02x}")).collect();
    let mut artifact = HitArtifact::new(
        ArtifactClass::SymmetricKey,
        format!("{token} (base64) = {hex}"),
    );
    if let Some(role) = role {
        artifact = artifact.role(role);
    }
    if let Some(detail) = detail {
        artifact = artifact.detail(detail);
    }
    hit.artifact(artifact)
}

/// Group carved tokens into per-section clusters of neighbours.
///
/// Tokens are grouped when they share a section and each starts within
/// [`KEY_CLUSTER_GAP`] bytes of the previous one — the layout of an encoded string
/// table. The input is in carving order (ascending address within each section), so
/// one forward pass suffices.
///
/// # Arguments
///
/// * `carved` - The carved tokens, in per-section ascending address order
fn cluster_carved_keys(carved: Vec<CarvedKey>) -> Vec<Vec<CarvedKey>> {
    let mut clusters: Vec<Vec<CarvedKey>> = Vec::new();
    for key in carved {
        // whether the token continues the open cluster's per-section run
        let continues = clusters
            .last()
            .and_then(|cluster| cluster.last())
            .is_some_and(|last| {
                last.section == key.section && key.addr.saturating_sub(last.addr) <= KEY_CLUSTER_GAP
            });
        match clusters.last_mut() {
            // extend the open cluster when the token continues its run
            Some(cluster) if continues => cluster.push(key),
            // otherwise the token starts a new cluster
            _ => clusters.push(vec![key]),
        }
    }
    clusters
}

/// Whether a cluster of carved tokens is reported as one string-table lead rather
/// than itemized candidate-key findings.
///
/// A cluster of [`KEY_CLUSTER_MIN`] or more is table-shaped on its own; and once the
/// image carves more than [`MAX_LONE_KEYS`] tokens in total, even small clusters
/// collapse — at that point itemized "candidate key" findings are a flood, not keys.
///
/// # Arguments
///
/// * `cluster_len` - The number of tokens in the cluster
/// * `total` - The number of tokens carved image-wide
fn cluster_collapses(cluster_len: usize, total: usize) -> bool {
    cluster_len >= KEY_CLUSTER_MIN || total > MAX_LONE_KEYS
}

/// Build the single collapsed lead for a cluster of carved key-length tokens.
///
/// The cluster is framed as what the evidence supports — an encoded/encrypted string
/// table whose entries all decode to key-length high-entropy blobs — rather than as N
/// independent symmetric keys, and carries no key artifact: nothing in it is a
/// validated key, and the artifact classes are pivot-class (they veto runtime
/// weakening and rollup suppression), which a speculative table lead has no claim to.
///
/// # Arguments
///
/// * `cluster` - The clustered tokens, in ascending address order
/// * `total` - The number of tokens carved image-wide (for the flood-context line)
/// * `function` - The function the cluster attributes to, when one references it
fn key_cluster_hit(cluster: &[CarvedKey], total: usize, function: Option<u64>) -> DetectionHit {
    // the cluster's address range anchors the lead and names the table
    let first = cluster.first().map_or(0, |key| key.addr);
    let last = cluster.last().map_or(0, |key| key.addr);
    let n = cluster.len();
    let section = cluster.first().map_or("", |key| key.section.as_str());
    // list the first few member addresses; the full list rides on the hit's sites
    let listed: Vec<String> = cluster
        .iter()
        .take(KEY_CLUSTER_LISTED)
        .map(|key| crate::thorpal::detect::context::fmt_addr(key.addr))
        .collect();
    let more = n.saturating_sub(KEY_CLUSTER_LISTED);
    let addr_list = match more {
        0 => listed.join(", "),
        more => format!("{} (+{more} more)", listed.join(", ")),
    };
    // adapt the headline to the cluster size (a singleton reaches here only in a
    // flooded image, where it reads as one more table entry rather than a key)
    let headline = if n > 1 {
        format!(
            "{n} same-shape base64 blobs at 0x{first:x}–0x{last:x} — likely an \
             encoded/encrypted string table, not {n} independent keys"
        )
    } else {
        format!(
            "a base64 key-length blob at 0x{first:x}, one of {total} carved from this \
             image — likely an entry of an encoded/encrypted string table rather than \
             an independent key"
        )
    };
    // a sample token gives the agent something concrete to decode by hand
    let sample = cluster
        .first()
        .map_or_else(String::new, |key| short_value(&key.token));
    // the collapsed content: the range, the count, and the sample token to pivot on
    let content = format!(
        "### Candidate Encoded String Table\n\n{headline}.\n\n### Metadata\n\
         - **Section:** {section}\n- **Range:** 0x{first:x}–0x{last:x}\n\
         - **Blobs:** {n} (of {total} carved image-wide)\n- **Addresses:** {addr_list}\n\
         - **Sample token:** `{sample}`\n"
    );
    let reasoning = "Each of these padded base64 tokens decodes to a standard key length \
         (16/24/32 bytes) of high-entropy bytes — which is also exactly what every entry of \
         a base64-wrapped encrypted string table looks like, and dozens of real embedded \
         keys in one run is not. Treat the run as one artifact: find the routine that walks \
         these entries (its decode is typically base64 then a short-keyed stream cipher such \
         as RC4) and recover its single table-wide key, rather than testing each blob as a \
         key."
        .to_string();
    DetectionHit::new(
        T1486,
        first,
        function,
        // a correctly-framed table pointer is reliable, but it is not a recovered key,
        // so it sits below the per-key candidate grade
        (Confidence::Likely, 60),
        content,
        reasoning,
    )
    .name(format!(
        "Encoded String Table (candidate) - {n} base64 blobs @ {section} @ 0x{first:x}"
    ))
    .kind(EvidenceKind::ContextOnly)
    .sites(cluster.iter().map(|key| key.addr).collect())
}

/// Build the flag `content` markdown for a recovered embedded symmetric key: the base64
/// token in a fenced block plus a metadata list (section, address, encoding, decoded hex,
/// entropy).
///
/// # Arguments
///
/// * `token` - The base64 key token as stored in the binary
/// * `section` - The section the key came from (e.g. `.rodata`)
/// * `addr` - The absolute address the key starts at
/// * `decoded` - The decoded raw key bytes
fn key_content(token: &str, section: &str, addr: u64, decoded: &[u8]) -> String {
    // render the decoded key as lower-case hex
    let hex: String = decoded.iter().map(|b| format!("{b:02x}")).collect();
    let mut out = String::new();
    out.push_str("### Candidate Symmetric Key\n\n");
    out.push_str("```\n");
    out.push_str(token);
    out.push_str("\n```\n");
    out.push_str("\n### Metadata\n");
    out.push_str(&format!("- **Section:** {section}\n"));
    out.push_str(&format!("- **Address:** 0x{addr:x}\n"));
    out.push_str(&format!(
        "- **Encoding:** base64 → {} bytes ({}-bit)\n",
        decoded.len(),
        decoded.len() * 8
    ));
    out.push_str(&format!("- **Decoded (hex):** {hex}\n"));
    out.push_str(&format!(
        "- **Entropy:** {:.2} bits/byte\n",
        decode::shannon_entropy(decoded)
    ));
    out
}

/// Carve candidate symmetric keys from raw section bytes.
///
/// Finds padded base64 tokens (`…=`/`…==`) that decode to a standard symmetric key length
/// (16/24/32 bytes) of high-entropy bytes, looking back at most [`MAX_KEY_B64_LEN`]
/// characters from each padding marker. Requires a `+`/`/` in the token so ordinary word
/// runs (which share the base64 alphabet) are not carved, and takes the tightest window
/// per padding marker. The base64 length arithmetic makes the padded window unambiguous
/// (e.g. with `==` padding only a 16-byte decode aligns to a 4-char boundary), so a key
/// glued to preceding text is still carved exactly. Returns `(offset, token, decoded)` per
/// recovered token. A 24-byte key (no base64 padding) is not carved here; it would be
/// picked up by [`collect_keys`] if the disassembler defined it as a string.
///
/// # Arguments
///
/// * `bytes` - The raw section bytes to scan
fn find_base64_keys(bytes: &[u8]) -> Vec<(usize, String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // grow a maximal standard-base64-alphabet run
        if !is_base64_std(bytes[i]) {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < bytes.len() && is_base64_std(bytes[i]) {
            i += 1;
        }
        let body_end = i;
        // count up to two '=' padding characters that anchor the base64 token
        let mut pad = 0;
        while i < bytes.len() && bytes[i] == b'=' && pad < 2 {
            pad += 1;
            i += 1;
        }
        // require padding as the base64 anchor (unpadded blobs are too ambiguous to carve)
        if pad == 0 {
            continue;
        }
        let body = &bytes[run_start..body_end];
        // only look back MAX_KEY_B64_LEN chars from the padding (a ≤32-byte key is short)
        let lo = body.len().saturating_sub(MAX_KEY_B64_LEN);
        for start in lo..body.len() {
            let win_body = &body[start..];
            // a real base64 token is a multiple of 4 characters including its padding
            if !(win_body.len() + pad).is_multiple_of(4) {
                continue;
            }
            // require a clear base64 marker so a plain word run is never carved
            if !win_body.iter().any(|b| *b == b'+' || *b == b'/') {
                continue;
            }
            // build the token text (body + padding) and decode it
            let mut token = String::from_utf8_lossy(win_body).into_owned();
            token.push_str(&"=".repeat(pad));
            let Some(decoded) = decode::base64::decode(&token) else {
                continue;
            };
            // keep only a standard key length of high-entropy (random) bytes
            if matches!(decoded.len(), 16 | 24 | 32)
                && decode::shannon_entropy(&decoded) >= MIN_KEY_ENTROPY
            {
                out.push((run_start + start, token, decoded));
                break;
            }
        }
    }
    out
}

/// Whether a byte is a standard base64 alphabet character (`A`–`Z` `a`–`z` `0`–`9` `+` `/`).
///
/// # Arguments
///
/// * `b` - The byte to classify
fn is_base64_std(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repeating key the test fixtures encrypt their plaintexts under.
    ///
    /// Every byte is high, so the ciphertext of printable text comes out non-printable
    /// the way real ciphertext does, and the key is long enough that the result is not
    /// one repeated value — a constant fixture would be rejected as padding by
    /// [`recover::is_structured_source`] before any of these tests reached what they
    /// are actually about.
    const TEST_KEY: &[u8] = &[0x9c, 0xa7, 0xb3, 0xd1, 0xe5];

    /// Encrypt a fixture plaintext under [`TEST_KEY`].
    ///
    /// # Arguments
    ///
    /// * `plain` - The fixture plaintext
    fn encrypt(plain: &[u8]) -> Vec<u8> {
        plain
            .iter()
            .enumerate()
            .map(|(idx, byte)| byte ^ TEST_KEY[idx % TEST_KEY.len()])
            .collect()
    }

    /// A long readable decryption is accepted; a short coincidental run is rejected.
    #[test]
    fn recovered_runs_requires_long_run() {
        // a long plaintext over non-printable ciphertext → a real decryption
        let plain = b"http://c2.example.com/gate.php?id=victim".to_vec();
        let ct = encrypt(&plain);
        assert!(recovered_runs(&plain, &ct, None).is_some());
        // a short readable run (< MIN_RECOVERY_CHARS) → a coincidental brute-force hit
        let short = b"abcdefghij".to_vec();
        let ct2 = encrypt(&short);
        assert!(recovered_runs(&short, &ct2, None).is_none());
    }

    /// A keyed-XOR result that just echoes the key is rejected (zero-source and the
    /// key-overlap guard).
    #[test]
    fn recovered_runs_rejects_key_echo() {
        let key = b"ABCDEFGHIJ";
        // decoded text shares a long run with the key, over non-printable source
        let plain = b"ABCDEFGHIJABCDEFGHIJABCD".to_vec();
        let ct = vec![0x80u8; plain.len()];
        assert!(recovered_runs(&plain, &ct, Some(key)).is_none());
        // zero source (key echoing through padding) is also rejected
        let ct_zero = vec![0u8; plain.len()];
        assert!(recovered_runs(&plain, &ct_zero, Some(key)).is_none());
    }

    /// A sparse source region echoes the key back in fragments, which the
    /// substring guard cannot see — the histogram can.
    #[test]
    fn recovered_runs_rejects_a_sparse_source() {
        // a lookup table: a long ramp of small, heavily repeated values
        let ct: Vec<u8> = (0..64u8).map(|i| i / 8).collect();
        let key = b"secret";
        let plain = decode::xor::keystream(key, &ct);
        assert!(recovered_runs(&plain, &ct, Some(key)).is_none());
        // ...while a genuinely encrypted source over the same key is not condemned.
        // It has to be real ciphertext to be a fair control: an arithmetic ramp mod 256
        // spreads its histogram evenly but still climbs between wraps, which is a
        // layout, and the structural tests are right to say so
        let hidden = decode::xor::keystream(
            key,
            b"POST /gate.php HTTP/1.1\r\nHost: cdn.example.net\r\nUser-Agent: mz/4\r\n",
        );
        assert!(!is_degenerate_source(&hidden));
        assert!(is_degenerate_source(&ct));
        assert!(is_degenerate_source(&[]));
    }

    /// An index table is rejected, and the histogram alone is not what rejects it.
    ///
    /// This is the shape that makes a printable key look like it decrypts, taken
    /// verbatim from a real binary's `.rdata`: consecutive slot numbers in short runs,
    /// spread over enough distinct values that no histogram bar stands out, and small
    /// enough that any printable key turns every one of them into a letter. Rejecting
    /// it on the *breadth* of the distribution is not open to this detection —
    /// printable plaintext under a printable key produces a mostly-control source too,
    /// and that is the case the whole module exists to find. So the guard that has to
    /// fire here is the consecutive-step test, and this test pins that division of
    /// labour rather than just the outcome.
    #[test]
    fn recovered_runs_rejects_an_index_table() {
        #[rustfmt::skip]
        let ct: Vec<u8> = vec![
            0x0c, 0x0d, 0x0e, 0x0f, 0x00, 0x01, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x0f, 0x0c, 0x0d, 0x0e, 0x0f, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
        ];
        // no value repeats often enough for the histogram bar to catch it...
        let mut counts = [0usize; 256];
        for byte in &ct {
            counts[*byte as usize] += 1;
        }
        assert!(
            counts
                .iter()
                .all(|count| *count < std::cmp::max(4, ct.len() / 8))
        );
        // ...so it is the stepping that condemns it
        assert!(recover::counts_up_or_down(&ct));
        assert!(is_degenerate_source(&ct));
        // and the fabricated output really is printable — that is the whole hazard
        let plain = decode::xor::keystream(b"\"Checking ProcessHeap.ForceFlags ", &ct);
        let printable = plain
            .iter()
            .filter(|b| b.is_ascii_graphic() || **b == b' ')
            .count();
        assert!(printable * 4 > plain.len() * 3, "{plain:?}");
        assert!(recovered_runs(&plain, &ct, Some(b"\"Checking ProcessHeap.ForceFlags ")).is_none());
    }

    /// The step test does not condemn a genuine keyed decryption.
    ///
    /// Real ciphertext steps by one about twice in every 256 adjacent pairs, and the
    /// awkward case — printable plaintext under a printable key, which is neither
    /// uniform nor broad — is barely worse. Both sit an order of magnitude under the
    /// bar, including the narrowest realistic shape there is: lowercase under
    /// lowercase, every byte inside a 32-wide window.
    #[test]
    fn step_test_keeps_genuine_keyed_ciphertext() {
        for (plain, key) in [
            (
                &b"http://c2.example.com/gate.php"[..],
                &b"bad allocation"[..],
            ),
            (
                br#"{"host":"10.1.2.3","port":443,"sleep":60}"#,
                b"bad allocation",
            ),
            (
                br"SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
                b"bad allocation",
            ),
            (b"payloadconfigurationdata", b"encryptionkey"),
        ] {
            let ct = decode::xor::keystream(key, plain);
            assert!(
                !recover::counts_up_or_down(&ct),
                "{}",
                String::from_utf8_lossy(plain)
            );
        }
    }

    /// A long run plus short fragments scattered through random bytes (a wrong-key
    /// brute-force pattern) is rejected by the coverage gate.
    #[test]
    fn recovered_runs_rejects_scattered_runs() {
        let mut plain = Vec::new();
        plain.extend_from_slice(b"http://c2.example.com/gate.php"); // 29-char run
        plain.extend(std::iter::repeat_n(0u8, 400)); // non-printable gap
        plain.extend_from_slice(b"another readable segment!!"); // a far-away run
        let ct = encrypt(&plain);
        // a long run passes the length bar, but the recovered text covers < 60% of its span
        assert!(recovered_runs(&plain, &ct, None).is_none());
    }

    /// `shares_substring` detects a common run of the given length.
    #[test]
    fn shares_substring_detects_overlap() {
        assert!(shares_substring("xxSECRETyy", "aSECRETb", 5));
        assert!(!shares_substring("abcdef", "ghijkl", 5));
    }

    /// `find_base64_keys` carves an embedded key glued into a concatenated blob (the Go
    /// string-blob case), returning exactly the key token and its decoded bytes.
    #[test]
    fn find_base64_keys_carves_embedded_key() {
        // the real embedded key from the RC6 sample, glued between neighbouring blob text
        // (no delimiters), exactly as it sits in the Go string blob
        let blob =
            b"9134674072265625ConstraintViolationErrorOHVbn3Fdv/sgvP9VRO/9OQ==Request Entity";
        let keys = find_base64_keys(blob);
        assert_eq!(keys.len(), 1);
        let (off, token, decoded) = &keys[0];
        assert_eq!(token, "OHVbn3Fdv/sgvP9VRO/9OQ==");
        // the token starts exactly at the key, not at the preceding glued text
        assert_eq!(&blob[*off..*off + token.len()], token.as_bytes());
        // 16-byte (128-bit) key decoding
        assert_eq!(decoded.len(), 16);
        assert_eq!(
            decoded
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "38755b9f715dbffb20bcff5544effd39"
        );
    }

    /// `find_base64_keys` ignores ordinary base64-alphabet text with no padded,
    /// key-length, high-entropy token (no `+`/`/`, not a key length).
    #[test]
    fn find_base64_keys_ignores_plain_text() {
        assert!(
            find_base64_keys(b"HelloWorldThisIsPlainAsciiTextWithNoEmbeddedKeyHere").is_empty()
        );
        // a padded base64 word that decodes to readable low-entropy text is not a key
        assert!(find_base64_keys(b"the value is dGhpcyBpcyBub3QgYSBrZXk=").is_empty());
    }

    /// A PEM block stored behind a base64 layer is carved and decodes back to armor,
    /// while a short run and non-base64 data yield nothing.
    #[test]
    fn find_base64_blobs_recovers_encoded_armor() {
        let pem = "-----BEGIN PUBLIC KEY-----\n\
                   MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAxYz1234567890abcdef\n\
                   -----END PUBLIC KEY-----\n";
        // the armored block, encoded once more and glued between unrelated bytes
        let encoded = decode::base64::encode(pem.as_bytes());
        let mut blob = b"\x00\x01\x02prefix\xff".to_vec();
        blob.extend_from_slice(encoded.as_bytes());
        blob.extend_from_slice(b"\xff\xfetrailing");
        let (found, skipped) = find_base64_blobs(&blob);
        // the decode budget is nowhere near spent on a blob this size
        assert_eq!(skipped, 0);
        // exactly one run is long enough to hold armor, and it decodes back to the PEM
        let recovered: Vec<_> = found
            .iter()
            .flat_map(|(_, decoded)| recover::find_pem_blocks(decoded))
            .collect();
        assert_eq!(recovered.len(), 1);
        assert!(recovered[0].1.contains("-----BEGIN PUBLIC KEY-----"));
        // a run shorter than an armored block is never decoded
        assert!(find_base64_blobs(b"c2hvcnQgcnVu").0.is_empty());
    }

    /// `test_label` matches a library's deliberate test-material armor and nothing else.
    #[test]
    fn test_label_matches_only_armor_phrases() {
        let testing = "-----BEGIN RSA TESTING KEY-----\nAAAA\n-----END RSA TESTING KEY-----";
        assert_eq!(test_label(testing), Some("testing key"));
        // a real key is untouched, even when its body happens to contain the letters
        let real = "-----BEGIN RSA PRIVATE KEY-----\ntestkeyAAAA\n-----END RSA PRIVATE KEY-----";
        assert_eq!(test_label(real), None);
        // a single word is not the phrase — "latest key" must not match "test key"
        assert_eq!(
            test_label("-----BEGIN LATEST KEYRING-----\nAA\n-----END X-----"),
            None
        );
    }

    /// `armor_der` renders copy-pasteable PEM whose body decodes back to the DER bytes.
    #[test]
    fn armor_der_round_trips_the_structure() {
        let raw: Vec<u8> = (0u8..=120).collect();
        let armored = armor_der(der::DerKind::Pkcs8PrivateKey, &raw);
        assert!(armored.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert!(armored.ends_with("\n-----END PRIVATE KEY-----"));
        // the body is wrapped at the PEM width and decodes back to the original bytes
        let body: String = armored
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");
        assert!(armored.lines().all(|line| line.len() <= PEM_LINE_WIDTH));
        assert_eq!(decode::base64::decode(&body).unwrap(), raw);
    }

    /// `key_attribution` separates an observed consumer, a bare reference, and no
    /// attribution at all — the three cases mean different things to an analyst.
    #[test]
    fn key_attribution_distinguishes_its_three_outcomes() {
        let mut consumers: KeyConsumers = HashMap::new();
        consumers.insert(0x1000, BTreeSet::from(["CryptImportKey"]));
        // a referencing function that also parses key material is a real consumer
        let (role, detail) = key_attribution(&consumers, Some(0x1000), "PEM block", 4096);
        assert_eq!(
            role.unwrap(),
            "references this PEM block and passes key material to CryptImportKey"
        );
        assert!(detail.is_none());
        // a referencing function with no key-parse call claims only the reference
        let (role, detail) = key_attribution(&consumers, Some(0x2000), "PEM block", 4096);
        assert_eq!(role.unwrap(), "references this PEM block");
        assert!(detail.unwrap().contains("no key-import or key-parse call"));
        // no referencing function must read as unattributed, never silently attributed
        let (role, detail) = key_attribution(&consumers, None, "DER structure", 4096);
        assert!(role.is_none());
        assert!(
            detail
                .unwrap()
                .contains("no function references this DER structure")
        );
    }

    /// `pem_artifact_value` carries the armor label plus the first body line, so two
    /// blocks sharing a label are still distinguishable in the artifact table.
    #[test]
    fn pem_artifact_value_names_the_label_and_body() {
        let block = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkq\nAAAA\n-----END PUBLIC KEY-----";
        assert_eq!(
            pem_artifact_value(block),
            "-----BEGIN PUBLIC KEY----- MIIBIjANBgkq"
        );
    }

    /// Each key kind maps to its own artifact class, so the report never merges a
    /// private key into the public-key inventory.
    #[test]
    fn artifact_class_separates_the_key_kinds() {
        assert_eq!(
            artifact_class(RecoveredKind::PrivateKey),
            ArtifactClass::AsymmetricPrivateKey
        );
        assert_eq!(
            artifact_class(RecoveredKind::PublicKey),
            ArtifactClass::AsymmetricPublicKey
        );
        assert_eq!(
            artifact_class(RecoveredKind::Certificate),
            ArtifactClass::Certificate
        );
        assert_eq!(
            artifact_class(RecoveredKind::SymmetricKey),
            ArtifactClass::SymmetricKey
        );
        // a non-key recovery is recorded as decoded data rather than mislabelled
        assert_eq!(
            artifact_class(RecoveredKind::Url),
            ArtifactClass::DecodedData
        );
    }

    /// `key_content` renders the token in a fenced block plus a metadata list with the
    /// decoded hex and bit length.
    #[test]
    fn key_content_renders_metadata() {
        let decoded = decode::base64::decode("OHVbn3Fdv/sgvP9VRO/9OQ==").unwrap();
        let md = key_content("OHVbn3Fdv/sgvP9VRO/9OQ==", ".rodata", 0x2bb2a9, &decoded);
        assert!(md.starts_with("### Candidate Symmetric Key\n"));
        assert!(md.contains("```\nOHVbn3Fdv/sgvP9VRO/9OQ==\n```"));
        assert!(md.contains("- **Section:** .rodata"));
        assert!(md.contains("- **Encoding:** base64 → 16 bytes (128-bit)"));
        assert!(md.contains("- **Decoded (hex):** 38755b9f715dbffb20bcff5544effd39"));
    }

    /// The stricter repeating-XOR text gate rejects the fleet's fabricated recoveries
    /// that clear the shared half-letters wordiness bar.
    #[test]
    fn xor_text_gate_rejects_fabricated_strings() {
        // real fabrications from the telemetry: FlipLockerESXi's 54%-letter keyboard
        // mash and a panic-byte XOR artifact — both pass `recover::is_wordy`
        assert!(!xor_run_is_wordy(b"$&'{b-ANjegQ+"));
        assert!(!xor_run_is_wordy(b"d`Lbrkh._,b%"));
        // end to end: a long printable mash decodes "cleanly" but is not text
        let plain = b"$&'{b-ANjegQ+#!x@Km=Pw%^rT~".to_vec();
        let ct = encrypt(&plain);
        // the shared gates alone accept it (so the RC4/RC6 branch still would)...
        assert!(recovered_runs(&plain, &ct, None).is_some());
        // ...and the XOR-only text gate is what rejects the fabrication
        assert!(recovered_runs(&plain, &ct, Some(b"secretkey")).is_none());
    }

    /// The confirmed recovery shape (92% letters, vowel-bearing word) still
    /// passes the stricter XOR text gate end to end.
    #[test]
    fn xor_text_gate_keeps_confirmed_recovery() {
        // a confirmed pattern: key "FindFirstUrlCacheEntryW" recovering
        // strings shaped like `N>trxQDihVDYs` (12/13 letters, vowel-bearing)
        assert!(xor_run_is_wordy(b"N>trxQDihVDYs"));
        // two such runs separated by a short non-printable gap, as in the real table
        let mut plain = b"N>trxQDihVDYs".to_vec();
        plain.extend_from_slice(&[0, 0, 0, 0]);
        plain.extend_from_slice(b"N>trxQDihVDYs!");
        let ct = encrypt(&plain);
        let runs = recovered_runs(&plain, &ct, Some(b"FindFirstUrlCacheEntryW"));
        assert!(runs.is_some());
    }

    /// Digit-substituted ransom prose passes the leet second chance while the
    /// keyboard-mash fabrications stay dead under both passes.
    #[test]
    fn xor_text_gate_gives_leet_prose_a_second_chance() {
        // real leetspeak ransom-note lines (the confirmed note shapes)
        assert!(xor_run_is_wordy(
            b"A44EN4ION! IN O2DE2 4O 02E6EN4 DA4A DAMAGE"
        ));
        assert!(xor_run_is_wordy(b"* DO NO4 MODIF9 ENC2904ED FILE3"));
        // the page's wrong-key printable fabrications fail both passes: no space
        // for the leet pass, and under two-thirds letters for the plain pass
        assert!(!xor_run_is_wordy(b"$&'{b-ANjegQ+"));
        assert!(!xor_run_is_wordy(b"d`Lbrkh._,b%"));
        // digit soup with spaces fails the real-letter third the leet pass demands
        assert!(!xor_run_is_wordy(b"44 020 9222 137 4479"));
    }

    /// Key↔ciphertext affinity holds for a shared referencing function or storage
    /// adjacency, and fails for a brute-force pairing with neither tie.
    #[test]
    fn key_ct_affinity_requires_shared_function_or_adjacency() {
        // a ciphertext buffer referenced by the function at 0x1000
        let ct = CtCandidate {
            addr: 0x9000,
            function: Some(0x1000),
            section: ".rodata".to_string(),
            bytes: vec![0x80; 32],
        };
        // the same function references the key: affine
        let shared = key_fixture(0x40_0000, Some(0x1000));
        assert!(key_ct_affinity(&shared, &ct));
        // a different function, but the key sits beside the buffer: affine
        let near = key_fixture(0x9800, Some(0x2000));
        assert!(key_ct_affinity(&near, &ct));
        // no shared function and far away: a brute-force pairing, demoted
        let stranger = key_fixture(0x40_0000, Some(0x2000));
        assert!(!key_ct_affinity(&stranger, &ct));
        // an unknown key address with no referencing function proves nothing
        let unknown = key_fixture(0, None);
        assert!(!key_ct_affinity(&unknown, &ct));
    }

    /// Build a key-candidate fixture at the given location.
    ///
    /// # Arguments
    ///
    /// * `addr` - The address the key string sits at (0 for unknown)
    /// * `function` - The entry of the function referencing the key, if any
    fn key_fixture(addr: u64, function: Option<u64>) -> KeyCandidate {
        KeyCandidate {
            bytes: b"CgeOvEO7hu".to_vec(),
            display: "\"CgeOvEO7hu\"".to_string(),
            addr,
            function,
        }
    }

    /// The decrypt-replay key artifact attaches only to key-class recoveries, so a
    /// plain recovered string no longer pivot-shields its hit.
    #[test]
    fn decrypt_key_artifact_only_for_key_kinds() {
        // the wrong-key fabrications are String-kind: no artifact, no pivot shield
        assert!(!carries_key_artifact(RecoveredKind::String));
        assert!(!carries_key_artifact(RecoveredKind::Url));
        assert!(!carries_key_artifact(RecoveredKind::Email));
        // key material validates the key that produced it and keeps the artifact
        assert!(carries_key_artifact(RecoveredKind::SymmetricKey));
        assert!(carries_key_artifact(RecoveredKind::PrivateKey));
        assert!(carries_key_artifact(RecoveredKind::PublicKey));
        assert!(carries_key_artifact(RecoveredKind::Certificate));
    }

    /// PEM validation rejects the writer-constant junk shapes: mismatched BEGIN/END
    /// labels, matching-label pairs with NUL bodies, and armor-only pairs.
    #[test]
    fn pem_validation_rejects_writer_constant_blocks() {
        // writer-constant junk: literally a BEGIN RSA / END EC pair
        let mismatched = "-----BEGIN RSA PRIVATE KEY-----\n-----END EC PRIVATE KEY-----";
        assert!(!pem_labels_match(mismatched));
        assert!(!is_valid_pem_block(mismatched));
        // the matching-label writer pair with NUL/garbage between the constants
        let nul_body = "-----BEGIN RSA PRIVATE KEY-----\n\u{0}\u{0}\u{0}\u{6}garbage\n-----END RSA PRIVATE KEY-----";
        assert!(pem_labels_match(nul_body));
        assert!(!has_pem_body(nul_body));
        assert!(!is_valid_pem_block(nul_body));
        // an armor-only pair holds no body at all
        let armor_only = "-----BEGIN RSA PRIVATE KEY-----\n-----END RSA PRIVATE KEY-----";
        assert!(!is_valid_pem_block(armor_only));
    }

    /// PEM validation keeps real recoveries: matching labels with decodable base64
    /// bodies, including a short single-line (Ed25519-sized) body.
    #[test]
    fn pem_validation_keeps_real_blocks() {
        // a classic wrapped body line (the PKCS#1/SPKI shape of the pack's real finds)
        let block = "-----BEGIN PUBLIC KEY-----\n\
                     MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAxYz1234567890abcdef\n\
                     -----END PUBLIC KEY-----";
        assert!(pem_labels_match(block));
        assert!(has_pem_body(block));
        assert!(is_valid_pem_block(block));
        // the shortest real-world body shape: one ~44-char line still qualifies
        let short_body = decode::base64::encode(&[7u8; 33]);
        let short = format!("-----BEGIN PUBLIC KEY-----\n{short_body}\n-----END PUBLIC KEY-----");
        assert!(is_valid_pem_block(&short));
    }

    /// Build a certificate PEM block whose body encodes the given DER-ish bytes.
    ///
    /// # Arguments
    ///
    /// * `der` - The bytes to armor as the block's body
    fn pem_cert_fixture(der: &[u8]) -> String {
        // wrap at the PEM width so the fixture matches a real recovered block's shape
        let body = decode::base64::wrap(&decode::base64::encode(der), PEM_LINE_WIDTH);
        format!("-----BEGIN CERTIFICATE-----\n{body}\n-----END CERTIFICATE-----")
    }

    /// Library-test and trust-store subjects in the decoded body are recognized;
    /// ordinary subjects are not.
    #[test]
    fn library_material_marker_matches_known_subjects() {
        // the PolarSSL test-suite CA that seeded spurious leads
        let polarssl = pem_cert_fixture(b"0\x82junk O=PolarSSL, CN=Polarssl Test EC CA junk");
        assert_eq!(library_material_marker(&polarssl), Some("polarssl"));
        // the DigiCert root that filled the ignored certificate flood
        let digicert = pem_cert_fixture(b"0\x82junk CN=DigiCert Assured ID Root CA junk");
        assert_eq!(library_material_marker(&digicert), Some("digicert"));
        // an ordinary subject keeps full severity (no demotion marker)
        let clean = pem_cert_fixture(b"0\x82junk CN=update.invalid, O=Example Ops junk");
        assert_eq!(library_material_marker(&clean), None);
    }

    /// A certificate flood collapses into one bundle lead at Unsure/40 with no
    /// pivot-class artifact; the shaping also caps itemized recoveries.
    #[test]
    fn pem_recovery_shaping_bundles_and_caps() {
        // ten recovered certificates plus one private key (a linked trust store)
        let mut recovered: Vec<(RecoveredKind, String, DetectionHit)> = (0..10u64)
            .map(|idx| {
                (
                    RecoveredKind::Certificate,
                    ".rdata".to_string(),
                    pem_hit_fixture(0x47_0000 + idx * 0x400),
                )
            })
            .collect();
        recovered.push((
            RecoveredKind::PrivateKey,
            ".rdata".to_string(),
            pem_hit_fixture(0x50_0000),
        ));
        let shaped = shape_pem_recoveries(recovered);
        // one bundle lead plus the itemized private key
        assert_eq!(shaped.len(), 2);
        let bundle = &shaped[0];
        assert!(matches!(bundle.confidence, Confidence::Unsure));
        assert_eq!(bundle.suspicion, 40);
        // sample-scoped, every member address attached, and no pivot-class artifact
        assert!(bundle.function.is_none());
        assert_eq!(bundle.sites.len(), 10);
        assert!(bundle.artifact.is_none());
        // the content says what it is and bounds the listed addresses
        assert!(
            bundle
                .content
                .contains("Embedded certificate bundle (10 certs)")
        );
        assert!(bundle.content.contains("(+2 more)"));
        assert!(bundle.name.as_deref().unwrap().contains("10 certificates"));
        // the private key stays itemized at its own grade
        assert_eq!(shaped[1].address, 0x50_0000);
        // a couple of certificates stay itemized, and a flood of keys is capped
        let few: Vec<_> = (0..2u64)
            .map(|idx| {
                (
                    RecoveredKind::Certificate,
                    ".rdata".to_string(),
                    pem_hit_fixture(0x1000 + idx),
                )
            })
            .collect();
        assert_eq!(shape_pem_recoveries(few).len(), 2);
        let flood: Vec<_> = (0..40u64)
            .map(|idx| {
                (
                    RecoveredKind::PrivateKey,
                    ".rdata".to_string(),
                    pem_hit_fixture(0x2000 + idx),
                )
            })
            .collect();
        assert_eq!(shape_pem_recoveries(flood).len(), MAX_PEM_HITS);
    }

    /// Build a minimal recovered-block hit fixture at the given address.
    ///
    /// # Arguments
    ///
    /// * `addr` - The address the block was recovered at
    fn pem_hit_fixture(addr: u64) -> DetectionHit {
        DetectionHit::new(
            T1486,
            addr,
            None,
            (Confidence::Fact, 60),
            "fixture",
            "fixture",
        )
    }

    /// The itemize-or-collapse decision: clusters of 3+ collapse, flooded images
    /// collapse everything, and lone carvings keep full severity.
    #[test]
    fn cluster_collapses_matches_the_table_shapes() {
        // the stealc table: 18 carvings in one run collapse into one lead
        assert!(cluster_collapses(18, 18));
        // the lone confirmed RC6 key stays an itemized candidate-key finding
        assert!(!cluster_collapses(1, 1));
        assert!(!cluster_collapses(2, 2));
        // a flooded image collapses even its small clusters
        assert!(cluster_collapses(2, 6));
        assert!(cluster_collapses(1, 99));
        // three same-shape neighbours are already table-shaped
        assert!(cluster_collapses(3, 3));
    }

    /// Build a carved-key fixture at the given location (the stealc token shape).
    ///
    /// # Arguments
    ///
    /// * `section` - The section the token was carved from
    /// * `addr` - The absolute address the token starts at
    fn carved_fixture(section: &str, addr: u64) -> CarvedKey {
        CarvedKey {
            section: section.to_string(),
            addr,
            token: "OHVbn3Fdv/sgvP9VRO/9OQ==".to_string(),
            decoded: decode::base64::decode("OHVbn3Fdv/sgvP9VRO/9OQ==").unwrap(),
        }
    }

    /// Carved tokens group per section within the cluster gap and split otherwise.
    #[test]
    fn cluster_carved_keys_groups_table_runs() {
        let carved = vec![
            carved_fixture(".rodata", 0x1000),
            carved_fixture(".rodata", 0x1100),
            carved_fixture(".rodata", 0x1a00),
            // a far-away token starts its own cluster
            carved_fixture(".rodata", 0x9_0000),
            // ...and a different section always does
            carved_fixture(".data", 0x9_0010),
        ];
        let clusters = cluster_carved_keys(carved);
        let sizes: Vec<usize> = clusters.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![3, 1, 1]);
    }

    /// A stealc-style cluster is reshaped into one correctly-framed string-table lead
    /// that keeps the table pointer the confirming agents used.
    #[test]
    fn key_cluster_hit_frames_the_string_table() {
        // five table entries starting at a confirmed stealc table address
        let cluster: Vec<CarvedKey> = (0..5u64)
            .map(|idx| carved_fixture(".rdata", 0x1_4007_da08 + idx * 0x78))
            .collect();
        let hit = key_cluster_hit(&cluster, 5, Some(0x1_4000_1000));
        // one Likely/60 lead framed as a table, not five Likely/85 key findings
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, 60);
        assert!(hit.content.contains("5 same-shape base64 blobs"));
        assert!(hit.content.contains("not 5 independent keys"));
        // the pointer survives: the table range and every member address
        assert!(hit.content.contains("0x14007da08"));
        assert_eq!(hit.sites.len(), 5);
        assert_eq!(hit.function, Some(0x1_4000_1000));
        // no key artifact: nothing here is a validated key, so no pivot shield
        assert!(hit.artifact.is_none());
        assert!(
            hit.name
                .as_deref()
                .unwrap()
                .contains("Encoded String Table")
        );
    }

    /// P-less Q-constant hits gain the hash-mixer/TEA framing note and the
    /// always-noise mark at unchanged severity; co-scanned non-Q weak hits are
    /// untouched.
    #[test]
    fn q_hits_annotated_when_p_is_absent() {
        // a disputed shape: Q matched inside a Fibonacci-multiplier immediate
        let mut hits = vec![
            DetectionHit::new(
                T1486,
                0x1_0023_0634,
                None,
                low(),
                "RC5/RC6/TEA magic constant Q (LE) constant @ 0x100230634 (section .text)",
                "Contains the golden-ratio constant Q (0x9E3779B9)",
            ),
            DetectionHit::new(
                T1486,
                0x2000,
                None,
                low(),
                "base64 standard alphabet constant @ 0x2000 (section .rodata)",
                "Contains the standard base64 alphabet",
            ),
        ];
        annotate_p_less_q_hits(&mut hits);
        // the Q hit now frames what a P-less match does and does not support
        assert!(hits[0].reasoning.contains("P (0xB7E15163) is absent"));
        assert!(hits[0].reasoning.contains("TEA"));
        // still seeded at the same weak severity (routing stays the seeder's call)
        assert!(matches!(hits[0].confidence, Confidence::Untrusted));
        assert_eq!(hits[0].suspicion, 15);
        // and marked as noise the seeder may roll up regardless of toolchain
        assert!(hits[0].always_noise);
        // the base64 hit is not a Q hit and is untouched
        assert!(!hits[1].reasoning.contains("absent"));
        assert!(!hits[1].always_noise);
    }

    /// The catalogs mark only the inherently noisy entries as always-noise.
    #[test]
    fn noise_assertions_cover_the_ubiquitous_entries() {
        // both base64 alphabets are noise wherever they appear; the Q constants
        // are not (the P-absence annotation decides that per image)
        for sig in WEAK_SIGS {
            let expected = sig.name.starts_with("base64");
            assert_eq!(sig.always_noise, expected, "{}", sig.name);
        }
        // the presence-only CryptoAPI utility calls are noise; the cipher calls are not
        for spec in APIS {
            let expected = matches!(spec.key, "CryptAcquireContext" | "CryptStringToBinary");
            assert_eq!(spec.always_noise, expected, "{}", spec.key);
        }
    }

    /// `CryptoAPI` reasonings are capability-neutral: the dual-use roles are named and
    /// no ransomware role is asserted.
    #[test]
    fn apis_use_capability_neutral_wording() {
        for spec in APIS {
            // the asserted-role claims that drew the disputes are gone
            assert!(
                !spec.reasoning.contains("core of ransomware"),
                "{}",
                spec.key
            );
            assert!(
                !spec.reasoning.contains("ransomware key-management"),
                "{}",
                spec.key
            );
            assert!(
                !spec.reasoning.contains("ransomware crypto scheme"),
                "{}",
                spec.key
            );
        }
        // the encrypt API names the dual-use roles and conditions the T1486 claim
        let encrypt = APIS.iter().find(|spec| spec.key == "CryptEncrypt").unwrap();
        assert!(encrypt.reasoning.contains("C2"));
        assert!(encrypt.reasoning.contains("T1486 only"));
        // key import names the C2 exchange the disputes kept pointing at
        let import = APIS
            .iter()
            .find(|spec| spec.key == "CryptImportKey")
            .unwrap();
        assert!(import.reasoning.contains("C2 session-key exchange"));
        // the provider-setup call admits it is capability-neutral
        let acquire = APIS
            .iter()
            .find(|spec| spec.key == "CryptAcquireContext")
            .unwrap();
        assert!(acquire.reasoning.contains("capability-neutral"));
    }
}
