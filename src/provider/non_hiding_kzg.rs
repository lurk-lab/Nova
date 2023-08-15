use std::{borrow::Borrow, marker::PhantomData, ops::Mul};

// KZG
use ff::{Field, PrimeField};
use group::{prime::PrimeCurveAffine, Curve, Group as _};
use pairing::{Engine, MillerLoopResult, MultiMillerLoop};
use rand::Rng;
use rand_core::{CryptoRng, RngCore};
use rayon::prelude::{IntoParallelIterator, ParallelIterator};

use crate::{errors::NovaError, traits::Group};

/// `UniversalParams` are the universal parameters for the KZG10 scheme.
#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub struct UVUniversalKZGParam<E: Engine> {
  /// Group elements of the form `{ \beta^i G }`, where `i` ranges from 0 to
  /// `degree`.
  pub powers_of_g: Vec<E::G1Affine>,
  /// The generator of G2.
  pub h: E::G2Affine,
  /// \beta times the above generator of G2.
  pub beta_h: E::G2Affine,
}

/// `UnivariateProverKey` is used to generate a proof
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct UVKZGProverKey<E: Engine> {
  /// generators
  pub powers_of_g: Vec<E::G1Affine>,
}

/// `UVKZGVerifierKey` is used to check evaluation proofs for a given
/// commitment.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct UVKZGVerifierKey<E: Engine> {
  /// The generator of G1.
  pub g: E::G1Affine,
  /// The generator of G2.
  pub h: E::G2Affine,
  /// \beta times the above generator of G2.
  pub beta_h: E::G2Affine,
}

impl<E: Engine> UVUniversalKZGParam<E>
where
  E::G1: Group,
{
  /// Returns the maximum supported degree
  pub fn max_degree(&self) -> usize {
    self.powers_of_g.len()
  }

  /// Returns the prover parameters
  ///
  /// # Panics
  /// if `supported_size` is greater than `self.max_degree()`
  pub fn extract_prover_key(&self, supported_size: usize) -> UVKZGProverKey<E> {
    let powers_of_g = self.powers_of_g[..=supported_size].to_vec();
    UVKZGProverKey { powers_of_g }
  }

  /// Returns the verifier parameters
  ///
  /// # Panics
  /// If self.prover_params is empty.
  pub fn extract_verifier_key(&self, supported_size: usize) -> UVKZGVerifierKey<E> {
    if self.powers_of_g.len() < supported_size {
      panic!("supported_size is greater than self.max_degree()");
    }
    UVKZGVerifierKey {
      g: self.powers_of_g[0],
      h: self.h,
      beta_h: self.beta_h,
    }
  }

  /// Trim the universal parameters to specialize the public parameters
  /// for univariate polynomials to the given `supported_size`, and
  /// returns prover key and verifier key. `supported_size` should
  /// be in range `1..params.len()`
  ///
  /// # Panics
  /// If `supported_size` is greater than `self.max_degree()`, or `self.max_degree()` is zero.
  pub fn trim(&self, supported_size: usize) -> (UVKZGProverKey<E>, UVKZGVerifierKey<E>) {
    let powers_of_g = self.powers_of_g[..=supported_size].to_vec();

    let pk = UVKZGProverKey { powers_of_g };
    let vk = UVKZGVerifierKey {
      g: self.powers_of_g[0],
      h: self.h,
      beta_h: self.beta_h,
    };
    (pk, vk)
  }

  /// Build SRS for testing.
  /// WARNING: THIS FUNCTION IS FOR TESTING PURPOSE ONLY.
  /// THE OUTPUT SRS SHOULD NOT BE USED IN PRODUCTION.
  pub fn gen_srs_for_testing<R: RngCore + CryptoRng>(mut rng: &mut R, max_degree: usize) -> Self {
    let beta = E::Fr::random(&mut rng);
    let g = E::G1::random(&mut rng);
    let h = E::G2::random(rng);

    let powers_of_g_projective = (0..=max_degree)
      .scan(g, |acc, _| {
        let val = *acc;
        *acc *= beta;
        Some(val)
      })
      .collect::<Vec<E::G1>>();

    let mut powers_of_g = vec![E::G1Affine::identity(); powers_of_g_projective.len()];
    E::G1::batch_normalize(&powers_of_g_projective, &mut powers_of_g);

    let h = h.to_affine();
    let beta_h = (h * beta).to_affine();

    let pp = Self {
      powers_of_g,
      h,
      beta_h,
    };
    pp
  }
}

/// Commitments
#[derive(Debug, Clone, Eq, PartialEq, Default)]
pub struct UVKZGCommitment<E: Engine>(
  /// the actual commitment is an affine point.
  pub E::G1Affine,
);

/// Polynomial Evaluation
pub struct UVKZGEvaluation<E: Engine>(E::Fr);

#[derive(Debug, Clone, Eq, PartialEq, Default)]

/// Proofs
pub struct UVKZGProof<E: Engine> {
  /// proof
  pub proof: E::G1Affine,
}

// TODO: we are extending this into a real Dense UV Polynomial type,
// and this is probably better organized elsewhere.
/// Polynomial and its associated types
pub type UVKZGPoly<F> = crate::spartan::sumcheck::UniPoly<F>;

impl<F: PrimeField> UVKZGPoly<F> {
  fn zero() -> Self {
    UVKZGPoly::new(Vec::new())
  }

  pub fn random<R: RngCore + CryptoRng>(degree: usize, mut rng: &mut R) -> Self {
    let coeffs = (0..=degree).map(|_| F::random(&mut rng)).collect();
    UVKZGPoly::new(coeffs)
  }

  /// Divide self by another polynomial, and returns the
  /// quotient and remainder.
  fn divide_with_q_and_r(&self, divisor: &Self) -> Option<(UVKZGPoly<F>, UVKZGPoly<F>)> {
    if self.is_zero() {
      Some((UVKZGPoly::zero(), UVKZGPoly::zero()))
    } else if divisor.is_zero() {
      panic!("Dividing by zero polynomial")
    } else if self.degree() < divisor.degree() {
      Some((UVKZGPoly::zero(), self.clone()))
    } else {
      // Now we know that self.degree() >= divisor.degree();
      let mut quotient = vec![F::ZERO; self.degree() - divisor.degree() + 1];
      let mut remainder: UVKZGPoly<F> = self.clone();
      // Can unwrap here because we know self is not zero.
      let divisor_leading_inv = divisor.leading_coefficient().unwrap().invert().unwrap();
      while !remainder.is_zero() && remainder.degree() >= divisor.degree() {
        let cur_q_coeff = *remainder.leading_coefficient().unwrap() * divisor_leading_inv;
        let cur_q_degree = remainder.degree() - divisor.degree();
        quotient[cur_q_degree] = cur_q_coeff;

        for (i, div_coeff) in divisor.coeffs.iter().enumerate() {
          remainder.coeffs[cur_q_degree + i] -= &(cur_q_coeff * div_coeff);
        }
        while let Some(true) = remainder.coeffs.last().map(|c| c == &F::ZERO) {
          remainder.coeffs.pop();
        }
      }
      Some((UVKZGPoly::new(quotient), remainder))
    }
  }
}

#[derive(Debug, Clone, Eq, PartialEq, Default)]
/// KZG Polynomial Commitment Scheme on univariate polynomial.
/// Note: this is non-hiding, which is why we will implement the EvaluationEngineTrait on this token struct,
/// as we will have several impls for the trait pegged on the same instance of a pairing::Engine.
pub struct UVKZGPCS<E> {
  #[doc(hidden)]
  phantom: PhantomData<E>,
}

impl<E: MultiMillerLoop> UVKZGPCS<E>
where
  E::G1: Group<PreprocessedGroupElement = E::G1Affine, Scalar = E::Fr>,
{
  // TODO: this relies on NovaError::InvalidIPA, which should really be extended to a sub-error enum
  // called "PCSError"
  /// Generate a commitment for a polynomial
  /// Note that the scheme is not hidding
  pub fn commit(
    prover_param: impl Borrow<UVKZGProverKey<E>>,
    poly: &UVKZGPoly<E::Fr>,
  ) -> Result<UVKZGCommitment<E>, NovaError> {
    let prover_param = prover_param.borrow();

    if poly.degree() > prover_param.powers_of_g.len() {
      return Err(NovaError::InvalidIPA);
    }
    let C = <E::G1 as Group>::vartime_multiscalar_mul(
      poly.coeffs.as_slice(),
      &prover_param.powers_of_g.as_slice()[..poly.coeffs.len()],
    );
    Ok(UVKZGCommitment(C.to_affine()))
  }

  /// Generate a commitment for a list of polynomials
  pub fn batch_commit(
    prover_param: impl Borrow<UVKZGProverKey<E>>,
    polys: &[UVKZGPoly<E::Fr>],
  ) -> Result<Vec<UVKZGCommitment<E>>, NovaError> {
    let prover_param = prover_param.borrow();

    polys
      .into_par_iter()
      .map(|poly| Self::commit(prover_param, poly))
      .collect::<Result<Vec<UVKZGCommitment<E>>, NovaError>>()
  }

  /// On input a polynomial `p` and a point `point`, outputs a proof for the
  /// same.
  pub fn open(
    prover_param: impl Borrow<UVKZGProverKey<E>>,
    polynomial: &UVKZGPoly<E::Fr>,
    point: &E::Fr,
  ) -> Result<(UVKZGProof<E>, UVKZGEvaluation<E>), NovaError> {
    let prover_param = prover_param.borrow();
    let divisor = UVKZGPoly {
      coeffs: vec![-*point, E::Fr::ONE],
    };
    // TODO: Better error
    let witness_polynomial = polynomial
      .divide_with_q_and_r(&divisor)
      .map(|(q, _r)| q)
      .ok_or(NovaError::InvalidIPA)?;
    let proof = <E::G1 as Group>::vartime_multiscalar_mul(
      witness_polynomial.coeffs.as_slice(),
      &prover_param.powers_of_g.as_slice()[..witness_polynomial.coeffs.len()],
    );
    let evaluation = UVKZGEvaluation(polynomial.evaluate(point));

    Ok((
      UVKZGProof {
        proof: proof.to_affine(),
      },
      evaluation,
    ))
  }

  /// Input a list of polynomials, and a same number of points,
  /// compute a multi-opening for all the polynomials.
  // This is a naive approach
  // TODO: to implement a more efficient batch opening algorithm
  // (e.g., the appendix C.4 in https://eprint.iacr.org/2020/1536.pdf)
  pub fn batch_open(
    prover_param: impl Borrow<UVKZGProverKey<E>>,
    polynomials: &[UVKZGPoly<E::Fr>],
    points: &[E::Fr],
  ) -> Result<(Vec<UVKZGProof<E>>, Vec<UVKZGEvaluation<E>>), NovaError> {
    if polynomials.len() != points.len() {
      // TODO: a better Error
      return Err(NovaError::InvalidIPA);
    }
    let mut batch_proof = vec![];
    let mut evals = vec![];
    for (poly, point) in polynomials.iter().zip(points.iter()) {
      let (proof, eval) = Self::open(prover_param.borrow(), poly, point)?;
      batch_proof.push(proof);
      evals.push(eval);
    }

    Ok((batch_proof, evals))
  }

  /// Verifies that `value` is the evaluation at `x` of the polynomial
  /// committed inside `comm`.
  pub fn verify(
    verifier_param: impl Borrow<UVKZGVerifierKey<E>>,
    commitment: &UVKZGCommitment<E>,
    point: &E::Fr,
    proof: &UVKZGProof<E>,
    evaluation: &UVKZGEvaluation<E>,
  ) -> Result<bool, NovaError> {
    let verifier_param = verifier_param.borrow();

    let pairing_inputs: Vec<(E::G1Affine, E::G2Prepared)> = vec![
      (
        (verifier_param.g.mul(evaluation.0) - proof.proof.mul(point) - commitment.0.to_curve())
          .to_affine(),
        verifier_param.h.into(),
      ),
      (proof.proof, verifier_param.beta_h.into()),
    ];
    let pairing_input_refs = pairing_inputs
      .iter()
      .map(|(a, b)| (a, b))
      .collect::<Vec<_>>();
    let pairing_result = E::multi_miller_loop(pairing_input_refs.as_slice()).final_exponentiation();
    Ok(pairing_result.is_identity().into())
  }

  /// Verifies that `value_i` is the evaluation at `x_i` of the polynomial
  /// `poly_i` committed inside `comm`.
  // This is a naive approach
  // TODO: to implement the more efficient batch verification algorithm
  // (e.g., the appendix C.4 in https://eprint.iacr.org/2020/1536.pdf)
  pub fn batch_verify<R: RngCore + CryptoRng>(
    verifier_params: impl Borrow<UVKZGVerifierKey<E>>,
    multi_commitment: &[UVKZGCommitment<E>],
    points: &[E::Fr],
    values: &[UVKZGEvaluation<E>],
    batch_proof: &[UVKZGProof<E>],
    rng: &mut R,
  ) -> Result<bool, NovaError> {
    let verifier_params = verifier_params.borrow();

    let mut total_c = <E::G1>::identity();
    let mut total_w = <E::G1>::identity();

    let mut randomizer = E::Fr::ONE;
    // Instead of multiplying g and gamma_g in each turn, we simply accumulate
    // their coefficients and perform a final multiplication at the end.
    let mut g_multiplier = E::Fr::ZERO;
    for (((c, z), v), proof) in multi_commitment
      .iter()
      .zip(points)
      .zip(values)
      .zip(batch_proof)
    {
      let w = proof.proof;
      let mut temp = w.mul(*z);
      temp += &c.0;
      let c = temp;
      g_multiplier += &(randomizer * v.0);
      total_c += &c.mul(randomizer);
      total_w += &w.mul(randomizer);
      // We don't need to sample randomizers from the full field,
      // only from 128-bit strings.
      randomizer = E::Fr::from_u128(rng.gen::<u128>());
    }
    total_c -= &verifier_params.g.mul(g_multiplier);

    let mut affine_points = vec![E::G1Affine::identity(); 2];
    E::G1::batch_normalize(&[-total_w, total_c], &mut affine_points);
    let (total_w, total_c) = (affine_points[0], affine_points[1]);

    let result = E::multi_miller_loop(&[
      (&total_w, &verifier_params.beta_h.into()),
      (&total_c, &verifier_params.h.into()),
    ])
    .final_exponentiation()
    .is_identity()
    .into();

    Ok(result)
  }
}

#[cfg(test)]
mod tests {
  use rand::{thread_rng, Rng};

  use super::*;

  fn end_to_end_test_template<E>() -> Result<(), NovaError>
  where
    E: MultiMillerLoop,
    E::G1: Group<PreprocessedGroupElement = E::G1Affine, Scalar = E::Fr>,
  {
    for _ in 0..100 {
      let mut rng = &mut thread_rng();
      let degree = rng.gen_range(2..20);

      let pp = UVUniversalKZGParam::<E>::gen_srs_for_testing(&mut rng, degree);
      let (ck, vk) = pp.trim(degree);
      let p = UVKZGPoly::random(degree, rng);
      let comm = UVKZGPCS::<E>::commit(&ck, &p)?;
      let point = E::Fr::random(rng);
      let (proof, value) = UVKZGPCS::<E>::open(&ck, &p, &point)?;
      assert!(
        UVKZGPCS::<E>::verify(&vk, &comm, &point, &proof, &value)?,
        "proof was incorrect for max_degree = {}, polynomial_degree = {}",
        degree,
        p.degree(),
      );
    }
    Ok(())
  }

  fn batch_check_test_template<E>() -> Result<(), NovaError>
  where
    E: MultiMillerLoop,
    E::G1: Group<PreprocessedGroupElement = E::G1Affine, Scalar = E::Fr>,
  {
    for _ in 0..10 {
      let mut rng = &mut thread_rng();

      let degree = rng.gen_range(2..20);

      let pp = UVUniversalKZGParam::<E>::gen_srs_for_testing(&mut rng, degree);
      let (ck, vk) = pp.trim(degree);

      let mut comms = Vec::new();
      let mut values = Vec::new();
      let mut points = Vec::new();
      let mut proofs = Vec::new();
      for _ in 0..10 {
        let mut rng = rng.clone();
        let p = UVKZGPoly::random(degree, &mut rng);
        let comm = UVKZGPCS::<E>::commit(&ck, &p)?;
        let point = E::Fr::random(rng);
        let (proof, value) = UVKZGPCS::<E>::open(&ck, &p, &point)?;

        assert!(UVKZGPCS::<E>::verify(&vk, &comm, &point, &proof, &value)?);
        comms.push(comm);
        values.push(value);
        points.push(point);
        proofs.push(proof);
      }
      assert!(UVKZGPCS::<E>::batch_verify(
        &vk, &comms, &points, &values, &proofs, &mut rng
      )?);
    }
    Ok(())
  }

  #[test]
  fn end_to_end_test() {
    end_to_end_test_template::<halo2curves::bn256::Bn256>().expect("test failed for Bn256");
  }

  #[test]
  fn batch_check_test() {
    batch_check_test_template::<halo2curves::bn256::Bn256>().expect("test failed for Bn256");
  }
}