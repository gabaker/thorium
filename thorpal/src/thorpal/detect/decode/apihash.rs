//! API-hash matching: resolve import hashes back to the names they hash.
//!
//! Shellcode and loaders resolve imports by hashing export names instead of
//! carrying the strings (ROR13 in the classic Metasploit resolver, FNV-1a and
//! multiply-rolling hashes in modern loaders, CRC32 and DJB2 everywhere). The
//! hash cannot be inverted, but it can be *matched*: hash every name in a
//! curated dictionary ([`super::apihash_names`]) under every variant a
//! resolver plausibly feeds it (case, UTF-16LE, trailing NUL, `Nt`/`Zw`
//! twins, stripped extensions, an optional salt) and report the names whose
//! hashes hit the targets.

use std::collections::HashSet;

use super::apihash_names::{API_NAMES, DLL_NAMES, PROCESS_NAMES};

/// The most matches one `match_hashes` call records
///
/// Weak families (`byte_sum` especially) can collide broadly; the cap keeps a
/// degenerate query from ballooning the result.
pub(crate) const MAX_HASH_MATCHES: usize = 400;

/// The hash families the matcher can replay
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashFamily {
    /// FNV-1a: `h = (h ^ c) * 0x01000193` from `0x811c9dc5`
    Fnv1a,
    /// A multiply-rolling hash: `h = h * mul + c` from `seed`
    MulRolling {
        /// The initial hash value
        seed: u32,
        /// The per-byte multiplier
        mul: u32,
    },
    /// The classic Metasploit resolver: `h = ror32(h, 13) + c` from zero
    Ror13,
    /// CRC-32 (IEEE, the `crc32`/zlib polynomial)
    Crc32,
    /// DJB2: `h = h * 33 + c` from 5381
    Djb2Add,
    /// DJB2-XOR: `h = (h * 33) ^ c` from 5381
    Djb2Xor,
    /// The plain byte sum (weak, but seen in tiny stagers)
    ByteSum,
}

impl HashFamily {
    /// The stable lowercase label this family is reported under
    pub(crate) fn label(self) -> &'static str {
        match self {
            HashFamily::Fnv1a => "fnv1a",
            HashFamily::MulRolling { .. } => "mul_rolling",
            HashFamily::Ror13 => "ror13",
            HashFamily::Crc32 => "crc32",
            HashFamily::Djb2Add => "djb2_add",
            HashFamily::Djb2Xor => "djb2_xor",
            HashFamily::ByteSum => "byte_sum",
        }
    }

    /// Hash one byte buffer under this family
    ///
    /// # Arguments
    ///
    /// * `bytes` - The (already variant-encoded, salted) bytes to hash
    pub(crate) fn hash(self, bytes: &[u8]) -> u32 {
        match self {
            // fnv1a xors the byte in, then multiplies by the prime
            HashFamily::Fnv1a => bytes.iter().fold(0x811c_9dc5u32, |h, c| {
                (h ^ u32::from(*c)).wrapping_mul(0x0100_0193)
            }),
            // the rolling hash multiplies, then adds the byte
            HashFamily::MulRolling { seed, mul } => bytes
                .iter()
                .fold(seed, |h, c| h.wrapping_mul(mul).wrapping_add(u32::from(*c))),
            // ror13 rotates right 13 bits, then adds the byte
            HashFamily::Ror13 => bytes
                .iter()
                .fold(0u32, |h, c| h.rotate_right(13).wrapping_add(u32::from(*c))),
            // crc32 is the zlib polynomial via crc32fast
            HashFamily::Crc32 => crc32fast::hash(bytes),
            // djb2 is the rolling hash with its classic constants
            HashFamily::Djb2Add => HashFamily::MulRolling {
                seed: 5381,
                mul: 33,
            }
            .hash(bytes),
            // the xor variant multiplies, then xors the byte in
            HashFamily::Djb2Xor => bytes
                .iter()
                .fold(5381u32, |h, c| h.wrapping_mul(33) ^ u32::from(*c)),
            // the byte sum just accumulates
            HashFamily::ByteSum => bytes
                .iter()
                .fold(0u32, |h, c| h.wrapping_add(u32::from(*c))),
        }
    }
}

/// Resolve a family-name string into the families to try
///
/// # Arguments
///
/// * `family` - The family name: one of `fnv1a`, `mul_rolling`, `ror13`,
///   `crc32`, `djb2_add`, `djb2_xor`, `byte_sum`, or `all`
/// * `seed` - The `mul_rolling` seed, required for that family
/// * `mul` - The `mul_rolling` multiplier, required for that family
pub(crate) fn families_from_str(
    family: &str,
    seed: Option<u32>,
    mul: Option<u32>,
) -> Result<Vec<HashFamily>, String> {
    // mul_rolling is the only parameterized family
    let rolling = match (seed, mul) {
        (Some(seed), Some(mul)) => Some(HashFamily::MulRolling { seed, mul }),
        (None, None) => None,
        // one parameter without the other is a mistake worth naming
        _ => {
            return Err(
                "`seed` and `multiplier` must be given together (they parameterize the \
                 mul_rolling family)"
                    .to_owned(),
            );
        }
    };
    // dispatch on the normalized family name
    let normalized = family.to_ascii_lowercase();
    // parameters on a family that cannot use them would be silently ignored,
    // which hides a mis-aimed query -- refuse instead
    if rolling.is_some() && normalized != "mul_rolling" && normalized != "all" {
        return Err(format!(
            "`seed`/`multiplier` only apply to the mul_rolling family (or \"all\", \
             which then includes it); family '{family}' does not use them"
        ));
    }
    match normalized.as_str() {
        "fnv1a" => Ok(vec![HashFamily::Fnv1a]),
        "ror13" => Ok(vec![HashFamily::Ror13]),
        "crc32" => Ok(vec![HashFamily::Crc32]),
        "djb2_add" => Ok(vec![HashFamily::Djb2Add]),
        "djb2_xor" => Ok(vec![HashFamily::Djb2Xor]),
        "byte_sum" => Ok(vec![HashFamily::ByteSum]),
        "mul_rolling" => match rolling {
            Some(family) => Ok(vec![family]),
            None => Err(
                "family \"mul_rolling\" needs `seed` and `multiplier` (e.g. seed \
                 0xC00018BE, multiplier 0x21)"
                    .to_owned(),
            ),
        },
        "all" => {
            // every parameterless family, plus mul_rolling when parameterized
            let mut families = vec![
                HashFamily::Fnv1a,
                HashFamily::Ror13,
                HashFamily::Crc32,
                HashFamily::Djb2Add,
                HashFamily::Djb2Xor,
                HashFamily::ByteSum,
            ];
            if let Some(family) = rolling {
                families.push(family);
            }
            Ok(families)
        }
        other => Err(format!(
            "unknown hash family '{other}'; supported: fnv1a, mul_rolling (with seed + \
             multiplier), ror13, crc32, djb2_add, djb2_xor, byte_sum, all"
        )),
    }
}

/// Which name variants the matcher tries per dictionary entry
///
/// Every flag defaults to true: trying all the variants is cheap and a
/// resolver's exact convention is rarely known up front.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NameVariants {
    /// Also hash the UTF-16LE encoding of each form
    pub wide: bool,
    /// Also hash the lowercased form (module names are usually lowered)
    pub lowercase: bool,
    /// Also hash the uppercased form
    pub uppercase: bool,
    /// Also hash the `Zw` twin of each `Nt`-prefixed name (and vice versa)
    pub nt_zw_twins: bool,
    /// Also hash names with their extension stripped (`kernel32.dll` -> `kernel32`)
    pub strip_ext: bool,
}

impl Default for NameVariants {
    /// All variants on: the exhaustive default the matcher is cheap enough for
    fn default() -> Self {
        NameVariants {
            wide: true,
            lowercase: true,
            uppercase: true,
            nt_zw_twins: true,
            strip_ext: true,
        }
    }
}

/// One resolved hash: the name (and variant of it) that hashes to the target
#[derive(Debug, Clone)]
pub(crate) struct HashMatch {
    /// The target hash that matched
    pub hash: u32,
    /// The dictionary (or extra) name whose variant hashes to it
    pub name: String,
    /// The family label the hash matched under
    pub family: &'static str,
    /// The variant of the name that produced the hash (e.g. `ascii+nul`)
    pub variant: String,
}

/// Build the textual forms of one name under the variant flags
///
/// Each form carries a label describing how it differs from the stored name;
/// the stored form itself is labeled `exact`. Forms are deduplicated so a
/// name that is already lowercase does not hash twice.
///
/// # Arguments
///
/// * `name` - The dictionary (or extra) name
/// * `variants` - The variant flags to expand under
fn textual_forms(name: &str, variants: &NameVariants) -> Vec<(String, String)> {
    // start from the stored form, then derive the structural variants
    let mut bases: Vec<(String, String)> = vec![(name.to_owned(), String::new())];
    // the Nt<->Zw twin covers syscall stubs without doubling the dictionary
    if variants.nt_zw_twins {
        if let Some(rest) = name.strip_prefix("Nt") {
            bases.push((format!("Zw{rest}"), "zw-twin".to_owned()));
        } else if let Some(rest) = name.strip_prefix("Zw") {
            bases.push((format!("Nt{rest}"), "nt-twin".to_owned()));
        }
    }
    // the extension-stripped form covers resolvers hashing bare module stems
    if variants.strip_ext
        && let Some(dot) = name.rfind('.')
        && dot > 0
    {
        bases.push((name[..dot].to_owned(), "no-ext".to_owned()));
    }
    // apply the case variants to every structural base, deduplicating
    let mut seen: HashSet<String> = HashSet::new();
    let mut forms = Vec::new();
    for (base, base_label) in bases {
        // helper-free label join: the base label plus the case label
        let mut push = |form: String, case: &str, forms: &mut Vec<(String, String)>| {
            if seen.insert(form.clone()) {
                let label = [base_label.as_str(), case]
                    .iter()
                    .filter(|part| !part.is_empty())
                    .copied()
                    .collect::<Vec<&str>>()
                    .join(" ");
                forms.push((form, label));
            }
        };
        push(base.clone(), "", &mut forms);
        if variants.lowercase {
            push(base.to_ascii_lowercase(), "lowercase", &mut forms);
        }
        if variants.uppercase {
            push(base.to_ascii_uppercase(), "uppercase", &mut forms);
        }
    }
    forms
}

/// Build the byte encodings of one textual form, salted
///
/// Every form is tried as ASCII and (when `wide`) UTF-16LE, each with and
/// without a trailing NUL -- resolvers are split roughly evenly on whether
/// they hash the terminator. The salt is prepended or appended to the final
/// byte buffer, matching how salted resolvers mix it in.
///
/// # Arguments
///
/// * `form` - The textual form to encode
/// * `wide` - Whether to also produce the UTF-16LE encodings
/// * `salt` - The optional salt bytes and whether they are a suffix
fn encoded_forms(
    form: &str,
    wide: bool,
    salt: Option<(&[u8], bool)>,
) -> Vec<(Vec<u8>, &'static str)> {
    let mut encoded = Vec::with_capacity(4);
    // the ascii encodings, bare and NUL-terminated
    encoded.push((form.as_bytes().to_vec(), "ascii"));
    let mut nul = form.as_bytes().to_vec();
    nul.push(0);
    encoded.push((nul, "ascii+nul"));
    // the utf16le encodings when wide forms are enabled
    if wide {
        let utf16: Vec<u8> = form.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut utf16_nul = utf16.clone();
        utf16_nul.extend_from_slice(&[0, 0]);
        encoded.push((utf16, "utf16le"));
        encoded.push((utf16_nul, "utf16le+nul"));
    }
    // mix the salt into every encoding, at whichever end the caller said
    if let Some((salt, suffix)) = salt {
        for (bytes, _) in &mut encoded {
            if suffix {
                bytes.extend_from_slice(salt);
            } else {
                let mut salted = salt.to_vec();
                salted.extend_from_slice(&*bytes);
                *bytes = salted;
            }
        }
    }
    encoded
}

/// Match target hashes against the curated dictionary plus extra names
///
/// Walks the dictionary once, hashing every variant of every name under every
/// family, and returns the matches plus the targets nothing resolved. Matches
/// are capped at [`MAX_HASH_MATCHES`] so weak families cannot balloon the
/// result.
///
/// # Arguments
///
/// * `targets` - The hash values to resolve
/// * `families` - The hash families to replay
/// * `variants` - The name variants to try per dictionary entry
/// * `salt` - Optional salt bytes and whether they suffix (true) or prefix
/// * `extra_names` - Caller-supplied names tried in addition to the dictionary
pub(crate) fn match_hashes(
    targets: &[u32],
    families: &[HashFamily],
    variants: &NameVariants,
    salt: Option<(&[u8], bool)>,
    extra_names: &[String],
) -> (Vec<HashMatch>, Vec<u32>) {
    // index the targets for O(1) membership tests during the walk
    let wanted: HashSet<u32> = targets.iter().copied().collect();
    let mut resolved: HashSet<u32> = HashSet::new();
    let mut matches: Vec<HashMatch> = Vec::new();
    // walk the three curated lists then the caller's extra names, once
    let dictionary = API_NAMES
        .iter()
        .chain(DLL_NAMES)
        .chain(PROCESS_NAMES)
        .copied()
        .chain(extra_names.iter().map(String::as_str));
    'names: for name in dictionary {
        // expand the name into its textual forms, then their byte encodings
        for (form, form_label) in textual_forms(name, variants) {
            for (bytes, enc_label) in encoded_forms(&form, variants.wide, salt) {
                // hash the encoding under every requested family
                for family in families {
                    let hash = family.hash(&bytes);
                    if wanted.contains(&hash) {
                        resolved.insert(hash);
                        // join the form and encoding labels into one variant tag
                        let variant = if form_label.is_empty() {
                            enc_label.to_owned()
                        } else {
                            format!("{form_label} {enc_label}")
                        };
                        matches.push(HashMatch {
                            hash,
                            name: name.to_owned(),
                            family: family.label(),
                            variant,
                        });
                        // stop the whole walk once the match cap fills
                        if matches.len() >= MAX_HASH_MATCHES {
                            break 'names;
                        }
                    }
                }
            }
        }
    }
    // the unresolved targets, deduplicated but in their given order
    let mut seen: HashSet<u32> = HashSet::new();
    let unmatched: Vec<u32> = targets
        .iter()
        .copied()
        .filter(|target| !resolved.contains(target) && seen.insert(*target))
        .collect();
    (matches, unmatched)
}

/// The number of names the matcher tries (dictionary plus extras)
///
/// # Arguments
///
/// * `extra_names` - The caller-supplied extra names
pub(crate) fn names_tried(extra_names: &[String]) -> usize {
    // the three curated lists plus whatever the caller added
    API_NAMES.len() + DLL_NAMES.len() + PROCESS_NAMES.len() + extra_names.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin each family against an independently-computed reference value.
    #[test]
    fn families_hash_known_vectors() {
        // fnv1a("GetProcAddress") computed with the standard constants
        let fnv = HashFamily::Fnv1a.hash(b"GetProcAddress");
        let mut reference = 0x811c_9dc5u32;
        for byte in b"GetProcAddress" {
            reference = (reference ^ u32::from(*byte)).wrapping_mul(0x0100_0193);
        }
        assert_eq!(fnv, reference);
        // ror13("x") from zero is just 'x'
        assert_eq!(HashFamily::Ror13.hash(b"x"), u32::from(b'x'));
        // crc32 of the classic check string is the published constant
        assert_eq!(HashFamily::Crc32.hash(b"123456789"), 0xCBF4_3926);
        // djb2_add("a") = 5381 * 33 + 'a'
        assert_eq!(HashFamily::Djb2Add.hash(b"a"), 5381 * 33 + u32::from(b'a'));
        // djb2_xor("a") = (5381 * 33) ^ 'a'
        assert_eq!(
            HashFamily::Djb2Xor.hash(b"a"),
            (5381 * 33) ^ u32::from(b'a')
        );
        // byte_sum("abc") is the plain sum
        assert_eq!(HashFamily::ByteSum.hash(b"abc"), 97 + 98 + 99);
    }

    /// `families_from_str` resolves every documented spelling and errors well.
    #[test]
    fn family_names_resolve() {
        assert_eq!(
            families_from_str("fnv1a", None, None).expect("fnv1a"),
            [HashFamily::Fnv1a]
        );
        // the family name is case-insensitive
        assert_eq!(
            families_from_str("ROR13", None, None).expect("ror13"),
            [HashFamily::Ror13]
        );
        // mul_rolling needs both parameters
        assert!(families_from_str("mul_rolling", None, None).is_err());
        assert!(families_from_str("fnv1a", Some(1), None).is_err());
        // parameters on a family that cannot use them are refused, not
        // silently ignored (only mul_rolling and "all" accept them)
        for family in ["fnv1a", "crc32", "ror13", "byte_sum"] {
            let error = families_from_str(family, Some(1), Some(2))
                .expect_err("stray parameters should be refused");
            assert!(error.contains("mul_rolling"), "unexpected error: {error}");
        }
        assert_eq!(
            families_from_str("mul_rolling", Some(7), Some(31)).expect("rolling"),
            [HashFamily::MulRolling { seed: 7, mul: 31 }]
        );
        // "all" is the six parameterless families, plus rolling when given
        assert_eq!(families_from_str("all", None, None).expect("all").len(), 6);
        assert_eq!(
            families_from_str("all", Some(7), Some(31))
                .expect("all")
                .len(),
            7
        );
        // an unknown family errors naming the supported set
        let error = families_from_str("sha256", None, None).expect_err("unknown");
        assert!(error.contains("fnv1a"), "unexpected error: {error}");
    }

    /// The variant logic derives Zw twins, cases, wide forms, and salts.
    #[test]
    fn variants_resolve_derived_forms() {
        let variants = NameVariants::default();
        // the Zw twin of NtClose matches without a ZwClose dictionary entry
        let zw_hash = HashFamily::Fnv1a.hash(b"ZwClose");
        let (matches, unmatched) =
            match_hashes(&[zw_hash], &[HashFamily::Fnv1a], &variants, None, &[]);
        assert!(unmatched.is_empty());
        assert!(
            matches
                .iter()
                .any(|hit| { hit.name == "NtClose" && hit.variant.contains("zw-twin") })
        );
        // the lowercase NUL-terminated utf16le form of a dll name matches
        let wide: Vec<u8> = "kernel32.dll"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .chain([0, 0])
            .collect();
        let wide_hash = HashFamily::Crc32.hash(&wide);
        let (matches, unmatched) =
            match_hashes(&[wide_hash], &[HashFamily::Crc32], &variants, None, &[]);
        assert!(unmatched.is_empty());
        assert!(
            matches
                .iter()
                .any(|hit| { hit.name == "kernel32.dll" && hit.variant.contains("utf16le+nul") })
        );
        // the extension-stripped uppercase form matches too
        let stem_hash = HashFamily::Djb2Add.hash(b"KERNEL32");
        let (matches, _) = match_hashes(&[stem_hash], &[HashFamily::Djb2Add], &variants, None, &[]);
        assert!(matches.iter().any(|hit| {
            hit.name == "kernel32.dll"
                && hit.variant.contains("no-ext")
                && hit.variant.contains("uppercase")
        }));
        // a suffix salt shifts the hash and still resolves when passed in
        let mut salted = b"LoadLibraryA".to_vec();
        salted.extend_from_slice(&[0xde, 0xad]);
        let salted_hash = HashFamily::Fnv1a.hash(&salted);
        let (matches, unmatched) = match_hashes(
            &[salted_hash],
            &[HashFamily::Fnv1a],
            &variants,
            Some((&[0xde, 0xad], true)),
            &[],
        );
        assert!(unmatched.is_empty());
        assert!(matches.iter().any(|hit| hit.name == "LoadLibraryA"));
    }

    /// Extra names extend the dictionary, and misses come back unmatched.
    #[test]
    fn extra_names_and_misses() {
        let variants = NameVariants::default();
        // a name no dictionary carries resolves through extra_names
        let custom = HashFamily::Ror13.hash(b"TotallyCustomExport");
        let (matches, unmatched) = match_hashes(
            &[custom, 0xdead_beef],
            &[HashFamily::Ror13],
            &variants,
            None,
            &["TotallyCustomExport".to_owned()],
        );
        assert!(matches.iter().any(|hit| hit.name == "TotallyCustomExport"));
        // the bogus target is reported unmatched, once
        assert_eq!(unmatched, [0xdead_beef]);
    }

    /// Disabled variant flags really disable their forms.
    #[test]
    fn disabled_variants_do_not_match() {
        // a wide-only hash misses when wide variants are off
        let wide: Vec<u8> = "NtClose"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let wide_hash = HashFamily::Fnv1a.hash(&wide);
        let narrow = NameVariants {
            wide: false,
            ..NameVariants::default()
        };
        let (matches, unmatched) =
            match_hashes(&[wide_hash], &[HashFamily::Fnv1a], &narrow, None, &[]);
        assert!(matches.is_empty());
        assert_eq!(unmatched, [wide_hash]);
    }
}
