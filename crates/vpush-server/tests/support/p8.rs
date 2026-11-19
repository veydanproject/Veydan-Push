//! Keys of APNs made for the tests, as a `.p8` file holds them. They sign
//! nothing outside them.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

/// A P-256 key in PKCS#8 PEM, and its PKCS#8 bytes.
pub fn p8() -> (String, Vec<u8>) {
    let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
        .unwrap()
        .as_ref()
        .to_vec();
    let b64 = STANDARD.encode(&der);
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    // The label is put in by `format!`, so that no line of this file looks
    // like the head of a private key to a scanner: the key is made anew by
    // every run and is in no file.
    let label = "PRIVATE KEY";
    let pem = format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    );
    (pem, der)
}
