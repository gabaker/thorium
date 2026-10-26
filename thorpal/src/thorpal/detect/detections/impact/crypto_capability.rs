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
        // the ASCII sigma constant "expand 32-byte k" used by ChaCha20/Salsa20
        bytes: b"expand 32-byte k",
        reasoning: "Contains the ChaCha20/Salsa20 constant \"expand 32-byte k\"; this stream cipher \
                    is used for file/session encryption (and CSPRNGs like rand_chacha). Strong \
                    evidence of a real crypto implementation.",
    },
    ByteSig {
        name: "AES forward S-box",
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
        // first four 32-bit H init values, big-endian
        bytes: &[
            0x6a, 0x09, 0xe6, 0x67, 0xbb, 0x67, 0xae, 0x85, 0x3c, 0x6e, 0xf3, 0x72, 0xa5, 0x4f,
            0xf5, 0x3a,
        ],
        reasoning: "Contains the SHA-256 initial hash values (H); evidence of SHA-256 hashing.",
    },
    ByteSig {
        name: "SHA-256 init hash (LE)",
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
        // first four 32-bit MD5 T[] sine constants, big-endian
        bytes: &[
            0xd7, 0x6a, 0xa4, 0x78, 0xe8, 0xc7, 0xb7, 0x56, 0x24, 0x20, 0x70, 0xdb, 0xc1, 0xbd,
            0xce, 0xee,
        ],
        reasoning: "Contains the MD5 T[] sine-constant table; evidence of MD5 hashing.",
    },
    ByteSig {
        name: "MD5 sine table (LE)",
        // first four 32-bit MD5 T[] sine constants, little-endian
        bytes: &[
            0x78, 0xa4, 0x6a, 0xd7, 0x56, 0xb7, 0xc7, 0xe8, 0xdb, 0x70, 0x20, 0x24, 0xee, 0xce,
            0xbd, 0xc1,
        ],
        reasoning: "Contains the MD5 T[] sine-constant table, little-endian; evidence of MD5 hashing.",
    },
    ByteSig {
        name: "SHA-1 init hash (BE)",
        // the five 32-bit SHA-1 init constants (fifth word disambiguates from MD5)
        bytes: &[
            0x67, 0x45, 0x23, 0x01, 0xef, 0xcd, 0xab, 0x89, 0x98, 0xba, 0xdc, 0xfe, 0x10, 0x32,
            0x54, 0x76, 0xc3, 0xd2, 0xe1, 0xf0,
        ],
        reasoning: "Contains the SHA-1 initial hash values; evidence of SHA-1 hashing.",
    },
    ByteSig {
        name: "SHA-1 init hash (LE)",
        // the five 32-bit SHA-1 init constants, little-endian
        bytes: &[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10, 0xf0, 0xe1, 0xd2, 0xc3,
        ],
        reasoning: "Contains the SHA-1 initial hash values, little-endian; evidence of SHA-1 hashing.",
    },
    ByteSig {
        name: "RC5/RC6 magic constant P (LE)",
        // the RC5/RC6 key-schedule constant P = 0xB7E15163, little-endian (x86 word storage)
        bytes: &[0x63, 0x51, 0xe1, 0xb7],
        reasoning: "Contains the RC5/RC6 key-schedule magic constant P (0xB7E15163, derived from \
                    Euler's number e); the seed of the RC5/RC6 round-key table. Evidence of an \
                    RC5/RC6 block cipher used for payload/config or file encryption.",
    },
    ByteSig {
        name: "RC5/RC6 magic constant P (BE)",
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
        bytes: b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
        reasoning: "Contains the standard base64 alphabet; base64 frequently wraps encrypted keys/\
                    payloads, including cipher data embedded in a dropped note. Ubiquitous, \
                    hence low confidence.",
    },
    ByteSig {
        name: "base64 URL-safe alphabet",
        bytes: b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
        reasoning: "Contains the URL-safe base64 alphabet; used to wrap encrypted keys/payloads for \
                    transport. Ubiquitous, hence low confidence.",
    },
    ByteSig {
        name: "RC5/RC6/TEA magic constant Q (LE)",
        // the RC5/RC6 key-schedule constant Q = 0x9E3779B9, little-endian
        bytes: &[0xb9, 0x79, 0x37, 0x9e],
        reasoning: "Contains the golden-ratio constant Q (0x9E3779B9) used by the RC5/RC6 key \
                    schedule; corroborates an RC5/RC6 cipher when seen alongside the P constant. \
                    Also used by the TEA/XTEA cipher family and many hash mixers, hence low \
                    confidence on its own.",
    },
    ByteSig {
        name: "RC5/RC6/TEA magic constant Q (BE)",
        // the RC5/RC6 key-schedule constant Q = 0x9E3779B9, big-endian
        bytes: &[0x9e, 0x37, 0x79, 0xb9],
        reasoning: "Contains the golden-ratio constant Q (0x9E3779B9) used by the RC5/RC6 key \
                    schedule; corroborates an RC5/RC6 cipher when seen alongside the P constant. \
                    Also used by the TEA/XTEA cipher family and many hash mixers, hence low \
                    confidence on its own.",
    },
];

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

/// Cryptographic APIs (Win32 CryptoAPI / CNG).
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "CryptEncrypt",
        severity: medium(),
        reasoning: "Calls CryptEncrypt (Win32 CryptoAPI) to encrypt data — a direct data-encryption \
                    capability, the core of ransomware file encryption.",
    },
    ApiSpec {
        key: "BCryptEncrypt",
        severity: medium(),
        reasoning: "Calls BCryptEncrypt (CNG) to encrypt data — a direct data-encryption capability.",
    },
    ApiSpec {
        key: "CryptDecrypt",
        severity: medium(),
        reasoning: "Calls CryptDecrypt (Win32 CryptoAPI); paired with encryption in ransomware, or \
                    used to decrypt an embedded payload/config.",
    },
    ApiSpec {
        key: "CryptImportKey",
        severity: medium(),
        reasoning: "Calls CryptImportKey to load a key blob (e.g. the attacker's embedded RSA public \
                    key used to wrap the file session key) — a ransomware key-management step.",
    },
    ApiSpec {
        key: "CryptGenKey",
        severity: medium(),
        reasoning: "Calls CryptGenKey to generate a symmetric/session key — the per-victim file \
                    encryption key in a ransomware crypto scheme.",
    },
    ApiSpec {
        key: "CryptAcquireContext",
        severity: low(),
        reasoning: "Calls CryptAcquireContext to open a CryptoAPI provider — the setup step before \
                    key generation / encryption. Common, hence low confidence.",
    },
    ApiSpec {
        key: "BCryptGenerateSymmetricKey",
        severity: medium(),
        reasoning: "Calls BCryptGenerateSymmetricKey (CNG) to create a symmetric key — the file \
                    encryption key in a ransomware crypto scheme.",
    },
    ApiSpec {
        key: "CryptStringToBinary",
        severity: low(),
        reasoning: "Calls CryptStringToBinary, often to decode a base64/PEM-wrapped embedded key or \
                    ciphertext into raw bytes for use with the crypto APIs.",
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
        hits.extend(
            signature_hits(ctx, T1486, WORD_SIGS, medium(), EvidenceKind::ContextOnly).await?,
        );
        hits.extend(signature_hits(ctx, T1486, WEAK_SIGS, low(), EvidenceKind::ContextOnly).await?);

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

/// A candidate key: its raw bytes plus a display rendering.
struct KeyCandidate {
    /// The key bytes fed to the cipher.
    bytes: Vec<u8>,
    /// A human rendering for the flag (`"…"` if printable, else hex).
    display: String,
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
/// [`recover::hidden_runs`]) are reported, so a wrong key — which yields random
/// bytes — never fires. Everything is bounded so this stays cheap.
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
    // collect candidate keys and ciphertext buffers (both bounded)
    let keys = collect_keys(ctx);
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
                    // a keyed decryption is reliable (Likely base); content sets
                    // suspicion and can only raise confidence further
                    let assessment = decode::assess(high().0, kind, &text);
                    let reasoning = decode::append_assessment(
                        decode::recovery_reasoning(CRYPTO_BASE_REASON, scheme, Some(&key.display)),
                        &assessment.notes,
                    );
                    // the key is the artifact here, and the decryption is the strongest
                    // attribution there is: this function's buffer decrypted with it
                    let mut artifact =
                        HitArtifact::new(ArtifactClass::SymmetricKey, key.display.clone()).detail(
                            format!(
                                "recovered by replaying {scheme} over the buffer at {}",
                                crate::thorpal::detect::context::fmt_addr(ct.addr)
                            ),
                        );
                    artifact = match ct.function {
                        Some(_) => artifact.role(format!(
                            "decrypts the buffer at {} with this key ({scheme})",
                            crate::thorpal::detect::context::fmt_addr(ct.addr)
                        )),
                        // the buffer decrypted, but no function was seen referencing it
                        None => artifact,
                    };
                    hits.push(
                        DetectionHit::new(
                            T1486,
                            addr,
                            ct.function,
                            (assessment.confidence, assessment.suspicion),
                            content,
                            reasoning,
                        )
                        .name(name)
                        .kind(EvidenceKind::ContextOnly)
                        .artifact(artifact),
                    );
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
/// # Arguments
///
/// * `ctx` - The analysis context
fn collect_keys(ctx: &AnalysisContext) -> Vec<KeyCandidate> {
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
        // 1) the raw string bytes as the key (an embedded plaintext password/key)
        push_key(
            &mut keys,
            &mut seen,
            bytes.to_vec(),
            format!("{:?}", string.value),
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
            push_key(&mut keys, &mut seen, decoded, display);
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
fn push_key(
    keys: &mut Vec<KeyCandidate>,
    seen: &mut HashSet<Vec<u8>>,
    bytes: Vec<u8>,
    display: String,
) {
    // skip empty or duplicate key material
    if bytes.is_empty() || !seen.insert(bytes.clone()) {
        return;
    }
    keys.push(KeyCandidate { bytes, display });
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

/// The max base64 characters looked back from a padding marker when carving an embedded
/// symmetric key (a ≤32-byte key encodes to ≤44 base64 characters). Bounds the scan.
const MAX_KEY_B64_LEN: usize = 44;

/// The minimum Shannon entropy (bits/byte) of a decoded blob to treat it as key material
/// (a random symmetric key is near the ~4-bit ceiling for 16 distinct bytes).
const MIN_KEY_ENTROPY: f32 = 3.5;

/// The max distance (bytes) from a referenced data address at which a carved key blob is
/// still attributed to the referencing function.
const KEY_REF_WINDOW: u64 = 4096;

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
/// `encoding/pem` template) still emits the boundary-present marker ([`pem_marker_hits`]),
/// preserving the capability signal without duplicating a recovered block. Dedup is by
/// recovered block **content**, shared across all three mechanisms.
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
    let mut hits = Vec::new();
    // dedup key = recovered block text, shared across all three mechanisms
    let mut seen: HashSet<String> = HashSet::new();

    // mechanism A: scan the raw bytes of every data-like section for full PEM blocks
    for (name, base, bytes) in &sections {
        if !decode::buffers::is_data_section(name) {
            continue;
        }
        for (off, block) in recover::find_pem_blocks(bytes) {
            let addr = base.saturating_add(off as u64);
            if let Some(hit) = emit_pem(ctx, &index, consumers, &mut seen, addr, name, block, None)
            {
                hits.push(hit);
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
            // a full block: emit typed content (dedup collapses it against mechanism A)
            Some((section, block)) => {
                if let Some(hit) = emit_pem(
                    ctx, &index, consumers, &mut seen, addr, section, block, None,
                ) {
                    hits.push(hit);
                }
            }
            // a lone boundary / runtime template: fall back to the boundary-present marker
            None => hits.extend(pem_marker_hits(ctx, &index, string).await?),
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
                if let Some(hit) = emit_pem(
                    ctx,
                    &index,
                    consumers,
                    &mut seen,
                    addr,
                    name,
                    block,
                    Some("base64"),
                ) {
                    hits.push(hit);
                }
            }
        }
    }
    Ok(hits)
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

/// Build one full-content flag for a recovered PEM block, or `None` when it is not a real
/// key/cert or is a duplicate.
///
/// Classifies the block and only surfaces a public key, private key, or certificate (a
/// bare boundary/template classifies as a generic string and is skipped), requires a
/// non-trivial body between the boundaries, and dedups by content via `seen`. The flag
/// carries the full block, a typed snippet-bearing name, and a `Fact` confidence — a
/// verbatim plaintext block has no decode uncertainty.
///
/// A block whose armor label or referencing function marks it as library/test material
/// is weakened and annotated rather than dropped: a standard library that statically
/// links an HTTP or TLS stack ships its own test key material into every binary built
/// against it, and silently discarding the class would also discard a real recovery
/// whenever the sample's own key happens to sit in the same company.
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
    block: String,
    encoding: Option<&str>,
) -> Option<DetectionHit> {
    // only a real key/cert armor is surfaced; a bare `-----BEGIN ` template is not
    let kind = decode::classify(&block);
    if !matches!(
        kind,
        RecoveredKind::PublicKey | RecoveredKind::PrivateKey | RecoveredKind::Certificate
    ) {
        return None;
    }
    // require key material between the boundaries (guards an armor-only BEGIN/END pair)
    if !has_pem_body(&block) {
        return None;
    }
    // dedup by content so no mechanism reports the same key/cert a second time
    if !seen.insert(block.clone()) {
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
    let name = decode::recovered_name(kind, &block, section, addr);
    let content = decode::recovered_content(kind, &block, section, addr, &stored, None);
    // a verbatim plaintext block has zero decode uncertainty → Fact; content sets
    // suspicion. A block recovered through a decode step is only as good as the decode,
    // so it starts at the keyed-recovery level instead
    let method = match encoding {
        Some(_) => high().0,
        None => Confidence::Fact,
    };
    let assessment = decode::assess(method, kind, &block);
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
    let mut reasoning = decode::append_assessment(base, &assessment.notes);
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
    let mut caveats = Vec::new();
    if let Some(label) = test_label(&block) {
        caveats.push(format!(
            "the PEM armor is labelled `{label}`, which standard libraries use to mark \
             deliberately non-production key material"
        ));
    }
    if let Some(fn_name) = function.and_then(|entry| ctx.function_name(entry)) {
        if is_library_function(fn_name) {
            caveats.push(format!(
                "the referencing function `{fn_name}` is statically linked runtime code \
                 rather than the program's own"
            ));
        }
    }
    if !caveats.is_empty() {
        hit.weaken();
        reasoning.push_str(&format!(
            "\n\nWeakened because {}. Reported rather than dropped: the block is genuinely \
             present, and suppressing the class would also hide a real key that happens to \
             sit alongside library material.",
            caveats.join(", and ")
        ));
        hit.reasoning = reasoning;
    }

    // tie the recovered key to the code that uses it (an unowned key is not triageable)
    let (role, detail) = key_attribution(consumers, function, "PEM block", PEM_REF_WINDOW);
    let mut artifact = HitArtifact::new(artifact_class(kind), pem_artifact_value(&block));
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
    Some(hit.artifact(artifact))
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

/// Whether a PEM block has key material (a non-empty, non-boundary line) between its
/// boundaries — rejecting an armor-only `-----BEGIN …----- -----END …-----` pair.
///
/// # Arguments
///
/// * `block` - The recovered PEM block text
fn has_pem_body(block: &str) -> bool {
    // any line that is neither blank nor a `-----…-----` boundary is body content
    block
        .lines()
        .map(str::trim)
        .any(|line| !line.is_empty() && !line.starts_with("-----"))
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
/// key-echo guard for keyed XOR and the strong-recovery gate.
///
/// Returns `None` when nothing survives (a wrong key). The `key_guard`, when set
/// (repeating XOR), rejects runs whose source region is mostly zero or that share a
/// long substring with the key — both signs of the key merely echoing through
/// low-entropy bytes rather than a real decryption. The caller de-duplicates and
/// splits the surviving runs into items.
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

/// Recover and flag embedded **symmetric key** material (ATT&CK T1486).
///
/// Malware bakes a symmetric key (RC6/AES/…) into the binary as a base64 blob, often
/// inside one concatenated string blob the disassembler never splits into a discrete
/// string (the Go/packed case — so [`collect_keys`], which reads defined strings, misses
/// it). This scans each data-like section's raw bytes for a padded base64 token that
/// decodes to a standard key length (16/24/32 bytes) of high-entropy bytes — a candidate
/// embedded key — and emits one flag per distinct key. It mirrors [`pem_hits`]'s raw-byte
/// recovery of *asymmetric* key material.
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
    let mut hits = Vec::new();
    // dedup by decoded key bytes, so the same key found twice is reported once
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (section, base, bytes) in &sections {
        // only real data sections hold embedded key material
        if !decode::buffers::is_data_section(section) {
            continue;
        }
        for (off, token, decoded) in find_base64_keys(bytes) {
            // one flag per distinct key
            if !seen.insert(decoded.clone()) {
                continue;
            }
            let addr = base.saturating_add(off as u64);
            // attribute the key to the function that references its buffer, when known
            let function = decode::buffers::referencing_function(&index, addr, KEY_REF_WINDOW);
            let bits = decoded.len() * 8;
            // typed, snippet-bearing name; full-content markdown; analyst reasoning
            let name = format!(
                "Symmetric Key (candidate) - \"{}\" @ {section} @ 0x{addr:x}",
                short_value(&token)
            );
            let content = key_content(&token, section, addr, &decoded);
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
            let assessment = decode::assess(high().0, RecoveredKind::SymmetricKey, &token);
            let reasoning = decode::append_assessment(base, &assessment.notes);
            let mut hit = DetectionHit::new(
                T1486,
                addr,
                function,
                (assessment.confidence, assessment.suspicion),
                content,
                reasoning,
            )
            .name(name)
            .kind(EvidenceKind::ContextOnly);
            // library-shipped material is weakened and annotated, never dropped
            if let Some(fn_name) = function.and_then(|entry| ctx.function_name(entry)) {
                if is_library_function(fn_name) {
                    hit.weaken();
                    hit.reasoning.push_str(&format!(
                        " Weakened because the referencing function `{fn_name}` is statically \
                         linked runtime code rather than the program's own."
                    ));
                }
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
            hits.push(hit.artifact(artifact));
        }
    }
    Ok(hits)
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
}
