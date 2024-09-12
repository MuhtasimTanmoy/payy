use halo2_base::halo2_proofs::{
    circuit::Cell,
    halo2curves::bn256::{Bn256, Fq, G1Affine},
};
use poseidon_circuit::Bn256Fr;
use snark_verifier::{
    loader::{
        self,
        halo2::halo2_ecc::{ecc::EccChip, fields::fp::FpConfig},
    },
    pcs::kzg::{Bdfg21, Kzg, KzgAs, KzgSuccinctVerifyingKey, LimbsEncoding},
    system, verifier,
};

use super::constants::{BITS, LIMBS, RATE, R_F, R_P, T};

pub type Svk = KzgSuccinctVerifyingKey<G1Affine>;
pub type PoseidonTranscript<L, S> =
    system::halo2::transcript::halo2::PoseidonTranscript<G1Affine, L, S, T, RATE, R_F, R_P>;

pub type Pcs = Kzg<Bn256, Bdfg21>;
pub type As = KzgAs<Pcs>;
pub type Plonk = verifier::Plonk<Pcs, LimbsEncoding<LIMBS, BITS>>;
pub type BaseFieldEccChip = snark_verifier_sdk::types::BaseFieldEccChip;
pub type Halo2Loader<'a> = loader::halo2::Halo2Loader<'a, G1Affine, BaseFieldEccChip>;
pub type SnarkInstanceColumnCells = Vec<Cell>;
