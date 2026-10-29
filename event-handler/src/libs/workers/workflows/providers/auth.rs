//! The sign-in pieces provider listings need: AWS request signing, RS256 tokens, and encodings
//!
//! Secrets passed to these functions are only read while a request is built and are never
//! logged or returned in an error.

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use chrono::{DateTime, Utc};
use reqwest::Url;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Map, Value};
use std::time::SystemTime;
use thorium::models::ModelSecret;

/// The characters base64url encodes each 6 bit group as
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// The uppercase hex digits percent-encoding writes
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Encode bytes as unpadded base64url
///
/// # Arguments
///
/// * `bytes` - The bytes to encode
#[must_use]
pub fn base64url(bytes: &[u8]) -> String {
    // encode each group of 3 bytes as 4 characters, leaving off the padding
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // pack the chunk into 24 bits
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let packed = (b0 << 16) | (b1 << 8) | b2;
        // write one character per 6 bits the chunk covers
        let chars = chunk.len() + 1;
        for index in 0..chars {
            let shift = 18 - 6 * index;
            out.push(char::from(BASE64URL[((packed >> shift) & 0x3f) as usize]));
        }
    }
    out
}

/// Percent-encode a string for a url path segment or query value, leaving only unreserved
/// characters as is
///
/// # Arguments
///
/// * `raw` - The string to encode
#[must_use]
pub fn percent_encode(raw: &str) -> String {
    // keep the unreserved characters and encode every other byte
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

/// The AWS keys a request is signed with
#[derive(Debug, Clone)]
pub struct AwsKeys {
    /// The access key id
    pub access_key_id: String,
    /// The secret access key
    pub secret: ModelSecret,
    /// The session token of temporary keys
    pub session_token: Option<ModelSecret>,
}

/// What an AWS request is signed for
#[derive(Debug, Clone, Copy)]
pub struct AwsScope<'a> {
    /// The region the request goes to
    pub region: &'a str,
    /// The name of the service the request goes to
    pub service: &'a str,
    /// When the request is signed
    pub time: DateTime<Utc>,
}

/// Sign a request with AWS Signature Version 4 and get the headers to add to it
///
/// The host, the date, the session token when there is one, and every extra header are signed.
/// The returned headers are the date, the session token, and the `authorization` header.
///
/// # Arguments
///
/// * `method` - The request's method
/// * `url` - The request's url
/// * `extra` - Other headers to send and sign, by lowercase name
/// * `body` - The request's body
/// * `keys` - The keys to sign with
/// * `scope` - The region, service, and time to sign for
pub fn sigv4_headers(
    method: &str,
    url: &Url,
    extra: &[(String, String)],
    body: &[u8],
    keys: &AwsKeys,
    scope: AwsScope<'_>,
) -> Result<Vec<(String, String)>, String> {
    // the signer reads the keys as an identity that only lives for this request
    let identity = Identity::from(Credentials::new(
        keys.access_key_id.clone(),
        keys.secret.expose(),
        keys.session_token
            .as_ref()
            .map(|token| token.expose().to_owned()),
        None,
        "thorium-model-credential",
    ));
    // sign for the scope's region, service, and time, signing the session token in a header
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(scope.region)
        .name(scope.service)
        .time(SystemTime::from(scope.time))
        .settings(SigningSettings::default())
        .build()
        .map_err(|err| format!("the request couldn't be signed: {err}"))?
        .into();
    // sign the request with the headers it's sent with
    let headers = extra
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()));
    let request = SignableRequest::new(method, url.as_str(), headers, SignableBody::Bytes(body))
        .map_err(|err| format!("the request couldn't be signed: {err}"))?;
    let (instructions, _) = sign(request, &params)
        .map_err(|err| format!("the request couldn't be signed: {err}"))?
        .into_parts();
    // return the headers the request must carry
    Ok(instructions
        .headers()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect())
}

/// Find the RSA private key in a PEM document and load it
///
/// # Arguments
///
/// * `document` - The PEM document, which may also hold certificates
pub fn rsa_key(document: &str) -> Result<RsaKeyPair, String> {
    // read every PEM block in the document
    let blocks =
        pem::parse_many(document).map_err(|err| format!("the PEM key can't be read: {err}"))?;
    // load the first private key, in PKCS#8 or PKCS#1 form
    for block in &blocks {
        let loaded = match block.tag() {
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(block.contents()),
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(block.contents()),
            _ => continue,
        };
        return loaded.map_err(|err| format!("the RSA private key was rejected: {err}"));
    }
    Err("the PEM document holds no RSA private key".to_owned())
}

/// Get the base64url SHA-1 thumbprint of the first certificate in a PEM document
///
/// # Arguments
///
/// * `document` - The PEM document holding the certificate
pub fn certificate_thumbprint(document: &str) -> Result<String, String> {
    // read every PEM block in the document
    let blocks = pem::parse_many(document)
        .map_err(|err| format!("the PEM certificate can't be read: {err}"))?;
    // hash the first certificate's DER
    let cert = blocks
        .iter()
        .find(|block| block.tag() == "CERTIFICATE")
        .ok_or_else(|| "the PEM document holds no certificate".to_owned())?;
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, cert.contents());
    Ok(base64url(digest.as_ref()))
}

/// Build an RS256 signed JSON web token
///
/// # Arguments
///
/// * `header` - The token's header fields, without `alg` and `typ`
/// * `claims` - The token's claims
/// * `key` - The key to sign with
pub fn rs256_jwt(
    header: &[(&str, &str)],
    claims: &Value,
    key: &RsaKeyPair,
) -> Result<String, String> {
    // every token is an RS256 JWT
    let mut fields = Map::new();
    fields.insert("alg".to_owned(), Value::from("RS256"));
    fields.insert("typ".to_owned(), Value::from("JWT"));
    for (name, value) in header {
        fields.insert((*name).to_owned(), Value::from(*value));
    }
    // encode the header and claims as the signed message
    let message = format!(
        "{}.{}",
        base64url(Value::Object(fields).to_string().as_bytes()),
        base64url(claims.to_string().as_bytes())
    );
    // sign the message
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "the token couldn't be signed".to_owned())?;
    Ok(format!("{message}.{}", base64url(&signature)))
}
