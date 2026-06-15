//! age (X25519) encryption helpers.
//!
//! bukagu never implements its own cryptography. These are thin **streaming**
//! wrappers over the vetted [`age`] crate: [`encrypt_to`] seals a writer to a
//! public [`Recipient`]; [`decrypt_with`] opens a reader using the private
//! [`Identity`]. Streaming (rather than `age::encrypt`/`decrypt` over a `Vec`)
//! keeps whole-archive plaintext out of memory.
//!
//! Because the backup model is asymmetric, the machine running bukagu only ever
//! holds a *recipient* (public key); decryption requires the identity the user
//! keeps on their website.

use std::io::{Read, Write};

use age::stream::{StreamReader, StreamWriter};
use age::x25519::{Identity, Recipient};
use anyhow::{Context, Result};

/// Wrap `writer` so bytes written to it are age-encrypted to `recipient`.
///
/// The caller **must** call [`StreamWriter::finish`] on the returned writer when
/// done writing — otherwise the final chunk is never flushed and the output is
/// truncated and will not decrypt.
pub fn encrypt_to<W: Write>(recipient: &Recipient, writer: W) -> Result<StreamWriter<W>> {
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(recipient as &dyn age::Recipient))
            .context("building the age encryptor")?;
    encryptor
        .wrap_output(writer)
        .context("writing the age header")
}

/// Wrap `reader` so reads yield the age-decrypted plaintext, opened with the
/// private `identity`. Errors if the file was not encrypted to the matching
/// recipient, or is truncated/corrupt.
pub fn decrypt_with<R: Read>(identity: &Identity, reader: R) -> Result<StreamReader<R>> {
    let decryptor = age::Decryptor::new(reader).context("reading the age header")?;
    decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .context("decrypting the backup (wrong key or corrupt file?)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypt `plaintext` to `recipient`, returning the full age ciphertext.
    fn seal(recipient: &Recipient, plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = encrypt_to(recipient, &mut out).unwrap();
        w.write_all(plaintext).unwrap();
        w.finish().unwrap();
        out
    }

    /// Decrypt `ciphertext` with `identity`, reading the plaintext to the end.
    fn open(identity: &Identity, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut r = decrypt_with(identity, ciphertext)?;
        let mut buf = Vec::new();
        r.read_to_end(&mut buf)?;
        Ok(buf)
    }

    #[test]
    fn encrypt_then_decrypt_roundtrips() {
        let id = Identity::generate();
        let recipient = id.to_public();
        let msg = b"bukagu secret source bytes \x00\x01\x02\xff";

        let ct = seal(&recipient, msg);
        assert_ne!(ct.as_slice(), msg.as_slice(), "ciphertext != plaintext");
        assert_eq!(open(&id, &ct).unwrap(), msg);
    }

    #[test]
    fn wrong_identity_cannot_decrypt() {
        let id = Identity::generate();
        let other = Identity::generate();
        let ct = seal(&id.to_public(), b"only for id");
        assert!(
            open(&other, &ct).is_err(),
            "a different identity must not decrypt"
        );
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let id = Identity::generate();
        let mut ct = seal(
            &id.to_public(),
            b"the quick brown fox jumps over the lazy dog",
        );
        // Flip a bit in the last byte (encrypted payload / auth tag region).
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(
            open(&id, &ct).is_err(),
            "tampered payload must fail authentication"
        );
    }
}
