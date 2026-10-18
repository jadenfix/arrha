use crate::ed25519;

pub struct Keypair {
    inner: ed25519::Keypair,
}

impl Keypair {
    #[must_use]
    pub fn generate() -> Self {
        Keypair {
            inner: ed25519::Keypair::generate(),
        }
    }

    #[must_use = "constructing a Keypair without binding it is a no-op"]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ed25519::Ed25519Error> {
        Ok(Keypair {
            inner: ed25519::Keypair::from_bytes(bytes)?,
        })
    }

    #[inline]
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        self.inner.public_key()
    }

    #[inline]
    #[must_use]
    pub fn secret_key(&self) -> Vec<u8> {
        self.inner.secret_key()
    }

    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.inner.sign(message)
    }

    /// Deterministic, PUBLICLY KNOWN keypair number `index` for local devnets.
    ///
    /// The secret is `SHA-256("aether-devnet-account" || index_le)`, so anyone
    /// can derive it: these accounts are only for local test networks (the
    /// genesis ceremony funds them; the CLI and load generator sign with
    /// them). Never fund them on a real network.
    #[must_use]
    pub fn devnet(index: u32) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"aether-devnet-account");
        hasher.update(index.to_le_bytes());
        let seed: [u8; 32] = hasher.finalize().into();
        Keypair {
            inner: ed25519::Keypair::from_bytes(&seed).expect("32-byte seed is always valid"),
        }
    }

    #[must_use]
    pub fn to_address(&self) -> [u8; 20] {
        use sha2::{Digest, Sha256};
        let pubkey = self.public_key();
        let hash = Sha256::digest(&pubkey);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash[..20]);
        addr
    }
}

#[cfg(test)]
mod devnet_tests {
    use super::Keypair;

    #[test]
    fn devnet_keys_are_deterministic_and_distinct() {
        assert_eq!(
            Keypair::devnet(0).public_key(),
            Keypair::devnet(0).public_key()
        );
        assert_ne!(
            Keypair::devnet(0).public_key(),
            Keypair::devnet(1).public_key()
        );
    }
}
