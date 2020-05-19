//! Algorithms for the Schnorr "adaptor signature" signature encryption.
//!
//! Adaptor signatures are a kind of signature encryption that is generated by
//! the signer and allows the signer (or anyone else who has seen the
//! ciphertext) to recover the decryption key from the decrypted signature.
use crate::{KeyPair, Schnorr, Signature};
use digest::{generic_array::typenum::U32, Digest};
use secp256kfun::{
    derive_nonce, g,
    hash::{Derivation, NonceHash},
    marker::*,
    s, Point, Scalar,
};
mod encrypted_signature;
pub use encrypted_signature::EncryptedSignature;

pub trait AdaptorSign {
    fn encrypted_sign(
        &self,
        signing_key: &KeyPair,
        encryption_key: &Point<impl Normalized, impl Secrecy>,
        message: &[u8],
        derivation: Derivation,
    ) -> EncryptedSignature;
}

impl<GT, CH, NH> AdaptorSign for Schnorr<GT, CH, NonceHash<NH>>
where
    CH: Digest<OutputSize = U32> + Clone,
    NH: Digest<OutputSize = U32> + Clone,
{
    fn encrypted_sign(
        &self,
        signing_key: &KeyPair,
        encryption_key: &Point<impl Normalized, impl Secrecy>,
        message: &[u8],
        derivation: Derivation,
    ) -> EncryptedSignature {
        let (x, X) = signing_key.as_tuple();
        let Y = encryption_key;

        let mut r = derive_nonce!(
            nonce_hash => self.nonce_hash,
            derivation => derivation,
            secret => x,
            public => [X, Y, message]
        );

        let R = g!(r * self.G + Y)
            // R_hat = r * G is sampled pseudorandomly for every Y which means R_hat + Y is also
            // be pseudoranodm and therefore will not be zero.
            // NOTE: Crucially we add Y to the nonce derivation to ensure this is true.
            .mark::<NonZero>()
            .expect("computationally unreachable");

        let (R, needs_negation) = R.into_point_with_y_choice::<SquareY>();
        // We correct r here but we can't correct the decryption key (y) so we
        // store in "needs_negation" whether the decryptor needs to negate their
        // key before decrypting it
        r.conditional_negate(needs_negation);

        let c = self.challenge(&R.to_xonly(), X, message);
        let s_hat = s!(r + c * x).mark::<Public>();

        EncryptedSignature {
            R,
            s_hat,
            needs_negation,
        }
    }
}

/// Extension trait adding the algorithms for the adaptor signature scheme to instances of [`Schnorr`].
pub trait Adaptor {
    fn verify_encrypted_signature(
        &self,
        verification_key: &Point<EvenY, impl Secrecy>,
        encryption_key: &Point<impl Normalized, impl Secrecy>,
        message: &[u8],
        ciphertext: &EncryptedSignature<impl Secrecy>,
    ) -> bool;

    fn decrypt_signature(
        &self,
        decryption_key: Scalar<impl Secrecy>,
        ciphertext: EncryptedSignature<impl Secrecy>,
    ) -> Signature;

    fn recover_decryption_key(
        &self,
        encryption_key: &Point<impl Normalized, impl Secrecy>,
        ciphertext: &EncryptedSignature<impl Secrecy>,
        signature: &Signature<impl Secrecy>,
    ) -> Option<Scalar>;
}

impl<GT, CH, NH> Adaptor for Schnorr<GT, CH, NH>
where
    CH: Digest<OutputSize = U32> + Clone,
{
    #[must_use]
    fn verify_encrypted_signature(
        &self,
        verification_key: &Point<EvenY, impl Secrecy>,
        encryption_key: &Point<impl Normalized, impl Secrecy>,
        message: &[u8],
        ciphertext: &EncryptedSignature<impl Secrecy>,
    ) -> bool {
        let EncryptedSignature {
            R,
            s_hat,
            needs_negation,
        } = ciphertext;
        let X = verification_key;
        let Y = encryption_key.clone().mark::<Normal>();

        //  needs_negation => R_hat = R + Y
        // !needs_negation => R_hat = R - Y
        let R_hat = g!(R + { Y.conditional_negate(!needs_negation) });

        let c = self.challenge(&R.to_xonly(), &X.to_xonly(), message);

        R_hat == g!(s_hat * self.G - c * X)
    }

    fn decrypt_signature(
        &self,
        decryption_key: Scalar<impl Secrecy>,
        ciphertext: EncryptedSignature<impl Secrecy>,
    ) -> Signature {
        let EncryptedSignature {
            R,
            s_hat,
            needs_negation,
        } = ciphertext;
        let mut y = decryption_key;
        y.conditional_negate(needs_negation);
        let s = s!(s_hat + y).mark::<Public>();

        Signature { s, R: R.to_xonly() }
    }

    fn recover_decryption_key(
        &self,
        encryption_key: &Point<impl Normalized, impl Secrecy>,
        ciphertext: &EncryptedSignature<impl Secrecy>,
        signature: &Signature<impl Secrecy>,
    ) -> Option<Scalar> {
        if signature.R != ciphertext.R {
            return None;
        }

        let EncryptedSignature {
            s_hat,
            needs_negation,
            ..
        } = ciphertext;
        let s = &signature.s;

        let mut y = s!(s - s_hat);
        y.conditional_negate(*needs_negation);
        let implied_encryption_key = g!(y * self.G);

        if implied_encryption_key == *encryption_key {
            Some(y.mark::<NonZero>().expect("unreachable"))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod test {

    use super::*;
    use secp256kfun::{G, TEST_SOUNDNESS};

    secp256kfun::test_plus_wasm! {
        fn end_to_end() {
            let schnorr = Schnorr::from_tag(b"adaptor_test");
            for _ in 0..TEST_SOUNDNESS {
                let signing_keypair = schnorr.keygen(Scalar::random(&mut rand::thread_rng()));
                let verification_key = signing_keypair.verification_key();
                let decryption_key = Scalar::random(&mut rand::thread_rng());
                let encryption_key = g!(decryption_key * G).mark::<Normal>();

                let message = b"give 100 coins to Bob";

                let ciphertext = schnorr.encrypted_sign(
                    &signing_keypair,
                    &encryption_key,
                    &message[..],
                    Derivation::Deterministic,
                );

                assert!(schnorr
                    .verify_encrypted_signature(
                        &signing_keypair.verification_key(),
                        &encryption_key,
                        &message[..],
                        &ciphertext,
                    ));

                let decryption_key = decryption_key.mark::<Public>();
                let signature = schnorr.decrypt_signature(decryption_key.clone(), ciphertext.clone());
                assert!(schnorr.verify(&verification_key, &message[..], &signature));
                let rec_decryption_key = schnorr
                    .recover_decryption_key(&encryption_key, &ciphertext, &signature)
                    .expect("recovery works");
                assert_eq!(rec_decryption_key, decryption_key);
            }
        }
    }
}
