use crate::{
    pcs::{
        multilinear::{err_too_many_variates, validate_input},
        Evaluation, PolynomialCommitmentScheme,
    },
    piop::sum_check::{
        classic::{ClassicSumCheck, CoefficientsProver},
        eq_xy_eval, SumCheck, VirtualPolynomial,
    },
    poly::multilinear::MultilinearPolynomial,
    util::{
        arithmetic::{
            div_ceil, fixed_base_msm, inner_product, variable_base_msm, window_size, window_table,
            Curve, Field, MultiMillerLoop, PrimeCurveAffine,
        },
        end_timer,
        expression::{Expression, Query, Rotation},
        parallel::{num_threads, parallelize, parallelize_iter},
        start_timer,
        transcript::{TranscriptRead, TranscriptWrite},
        Itertools,
    },
    Error,
};
use num_integer::Integer;
use rand::RngCore;
use std::{
    borrow::Cow,
    iter,
    marker::PhantomData,
    ops::{Deref, Neg},
};

#[derive(Clone, Debug)]
pub struct MultilinearKzg<M: MultiMillerLoop>(PhantomData<M>);

#[derive(Clone, Debug)]
pub struct MultilinearKzgParams<M: MultiMillerLoop> {
    g1: M::G1Affine,
    eqs: Vec<Vec<M::G1Affine>>,
    g2: M::G2Affine,
    ss: Vec<M::G2Affine>,
}

impl<M: MultiMillerLoop> MultilinearKzgParams<M> {
    pub fn num_vars(&self) -> usize {
        self.eqs.len()
    }

    pub fn g1(&self) -> M::G1Affine {
        self.g1
    }

    pub fn eqs(&self) -> &[Vec<M::G1Affine>] {
        &self.eqs
    }

    pub fn g2(&self) -> M::G2Affine {
        self.g2
    }

    pub fn ss(&self) -> &[M::G2Affine] {
        &self.ss
    }
}

#[derive(Clone, Debug)]
pub struct MultilinearKzgProverParams<M: MultiMillerLoop> {
    g1: M::G1Affine,
    eqs: Vec<Vec<M::G1Affine>>,
}

impl<M: MultiMillerLoop> MultilinearKzgProverParams<M> {
    pub fn num_vars(&self) -> usize {
        self.eqs.len()
    }

    pub fn g1(&self) -> M::G1Affine {
        self.g1
    }

    pub fn eqs(&self) -> &[Vec<M::G1Affine>] {
        &self.eqs
    }

    pub fn eq(&self, num_vars: usize) -> &[M::G1Affine] {
        &self.eqs[self.num_vars() - num_vars]
    }
}

#[derive(Clone, Debug)]
pub struct MultilinearKzgVerifierParams<M: MultiMillerLoop> {
    g1: M::G1Affine,
    g2: M::G2Affine,
    ss: Vec<M::G2Affine>,
}

impl<M: MultiMillerLoop> MultilinearKzgVerifierParams<M> {
    pub fn num_vars(&self) -> usize {
        self.ss.len()
    }

    pub fn g1(&self) -> M::G1Affine {
        self.g1
    }

    pub fn g2(&self) -> M::G2Affine {
        self.g2
    }

    pub fn ss(&self, num_vars: usize) -> &[M::G2Affine] {
        &self.ss[self.num_vars() - num_vars..]
    }
}

#[derive(Clone, Debug)]
pub struct MultilinearKzgCommitment<M: MultiMillerLoop>(pub M::G1Affine);

impl<M: MultiMillerLoop> Default for MultilinearKzgCommitment<M> {
    fn default() -> Self {
        Self(M::G1Affine::identity())
    }
}

impl<M: MultiMillerLoop> AsRef<M::G1Affine> for MultilinearKzgCommitment<M> {
    fn as_ref(&self) -> &M::G1Affine {
        &self.0
    }
}

impl<M: MultiMillerLoop> PolynomialCommitmentScheme<M::Scalar> for MultilinearKzg<M> {
    type Param = MultilinearKzgParams<M>;
    type ProverParam = MultilinearKzgProverParams<M>;
    type VerifierParam = MultilinearKzgVerifierParams<M>;
    type Polynomial = MultilinearPolynomial<M::Scalar>;
    type Point = Vec<M::Scalar>;
    type Commitment = M::G1Affine;
    type CommitmentWithAux = MultilinearKzgCommitment<M>;

    fn setup(size: usize, mut rng: impl RngCore) -> Result<Self::Param, Error> {
        assert!(size.is_power_of_two());
        let num_vars = size.ilog2() as usize;
        let ss = iter::repeat_with(|| M::Scalar::random(&mut rng))
            .take(num_vars)
            .collect_vec();

        let expand_serial = |evals: &mut [M::Scalar], last_evals: &[M::Scalar], s_i: &M::Scalar| {
            for (evals, last_eval) in evals.chunks_mut(2).zip(last_evals.iter()) {
                evals[1] = *last_eval * s_i;
                evals[0] = *last_eval - &evals[1];
            }
        };

        let g1 = M::G1Affine::generator();
        let eqs = {
            let mut eqs = Vec::with_capacity(num_vars);
            let init_evals = vec![M::Scalar::one()];
            for s_i in ss.iter().rev() {
                let last_evals = eqs.last().unwrap_or(&init_evals);
                let mut evals = vec![M::Scalar::zero(); 2 * last_evals.len()];

                if evals.len() < 32 {
                    expand_serial(&mut evals, last_evals, s_i);
                } else {
                    let mut chunk_size = div_ceil(evals.len(), num_threads());
                    if chunk_size.is_odd() {
                        chunk_size += 1;
                    }
                    parallelize_iter(
                        evals
                            .chunks_mut(chunk_size)
                            .zip(last_evals.chunks(chunk_size >> 1)),
                        |(evals, last_evals)| expand_serial(evals, last_evals, s_i),
                    );
                }

                eqs.push(evals)
            }

            let window_size = window_size((2 << num_vars) - 2);
            let window_table = window_table(window_size, g1);
            let eqs_projective = fixed_base_msm(
                window_size,
                &window_table,
                eqs.iter().rev().flat_map(|evals| evals.iter()),
            );

            let mut eqs = vec![M::G1Affine::identity(); eqs_projective.len()];
            parallelize(&mut eqs, |(eqs, start)| {
                M::G1::batch_normalize(&eqs_projective[start..(start + eqs.len())], eqs);
            });
            let eqs = &mut eqs.drain(..);
            (0..num_vars)
                .map(move |idx| eqs.take(1 << (num_vars - idx)).collect_vec())
                .collect_vec()
        };

        let g2 = M::G2Affine::generator();
        let ss = {
            let window_size = window_size(num_vars);
            let window_table = window_table(window_size, M::G2Affine::generator());
            let ss_projective = fixed_base_msm(window_size, &window_table, &ss);

            let mut ss = vec![M::G2Affine::identity(); ss_projective.len()];
            parallelize(&mut ss, |(ss, start)| {
                M::G2::batch_normalize(&ss_projective[start..(start + ss.len())], ss);
            });
            ss
        };

        Ok(Self::Param { g1, eqs, g2, ss })
    }

    fn trim(
        param: &Self::Param,
        size: usize,
    ) -> Result<(Self::ProverParam, Self::VerifierParam), Error> {
        assert!(size.is_power_of_two());
        let num_vars = size.ilog2() as usize;
        if param.num_vars() < num_vars {
            return Err(err_too_many_variates("trim", param.num_vars(), num_vars));
        }
        let pp = Self::ProverParam {
            g1: param.g1,
            eqs: param.eqs[param.num_vars() - num_vars..].to_vec(),
        };
        let vp = Self::VerifierParam {
            g1: param.g1,
            g2: param.g2,
            ss: param.ss[param.num_vars() - num_vars..].to_vec(),
        };
        Ok((pp, vp))
    }

    fn commit(
        pp: &Self::ProverParam,
        poly: &Self::Polynomial,
    ) -> Result<Self::CommitmentWithAux, Error> {
        validate_input("commit", pp.num_vars(), [poly], None)?;

        Ok(variable_base_msm(poly.evals(), pp.eq(poly.num_vars())).into())
            .map(MultilinearKzgCommitment)
    }

    fn batch_commit<'a>(
        pp: &Self::ProverParam,
        polys: impl IntoIterator<Item = &'a Self::Polynomial>,
    ) -> Result<Vec<Self::CommitmentWithAux>, Error> {
        let polys = polys.into_iter().collect_vec();
        if polys.is_empty() {
            return Ok(Vec::new());
        }
        validate_input("batch commit", pp.num_vars(), polys.iter().copied(), None)?;

        Ok(polys
            .iter()
            .map(|poly| variable_base_msm(poly.evals(), pp.eq(poly.num_vars())).into())
            .map(MultilinearKzgCommitment)
            .collect())
    }

    fn open(
        pp: &Self::ProverParam,
        poly: &Self::Polynomial,
        _: &Self::CommitmentWithAux,
        point: &Self::Point,
        eval: &M::Scalar,
        transcript: &mut impl TranscriptWrite<M::G1Affine, M::Scalar>,
    ) -> Result<(), Error> {
        validate_input("open", pp.num_vars(), [poly], [point])?;

        let mut remainder = poly.evals().to_vec();
        let quotients = point
            .iter()
            .enumerate()
            .map(|(idx, x_i)| {
                let timer = start_timer(|| "quotients");
                let mut quotient = vec![M::Scalar::zero(); remainder.len() >> 1];
                parallelize(&mut quotient, |(quotient, start)| {
                    for (quotient, (remainder_0, remainder_1)) in quotient.iter_mut().zip(
                        remainder[2 * start..]
                            .iter()
                            .step_by(2)
                            .zip(remainder[2 * start + 1..].iter().step_by(2)),
                    ) {
                        *quotient = *remainder_1 - remainder_0;
                    }
                });

                let mut next_remainder = vec![M::Scalar::zero(); remainder.len() >> 1];
                parallelize(&mut next_remainder, |(next_remainder, start)| {
                    for (next_remainder, (remainder_0, remainder_1)) in
                        next_remainder.iter_mut().zip(
                            remainder[2 * start..]
                                .iter()
                                .step_by(2)
                                .zip(remainder[2 * start + 1..].iter().step_by(2)),
                        )
                    {
                        *next_remainder = (*remainder_1 - remainder_0) * x_i + remainder_0;
                    }
                });
                remainder = next_remainder;
                end_timer(timer);

                if quotient.len() == 1 {
                    variable_base_msm(&quotient, &[pp.g1]).into()
                } else {
                    variable_base_msm(&quotient, pp.eq(poly.num_vars() - idx - 1)).into()
                }
            })
            .collect_vec();

        if cfg!(feature = "sanity-check") {
            assert_eq!(&remainder[0], eval);
        }

        transcript.write_commitments(&quotients)?;

        Ok(())
    }

    fn batch_open<'a>(
        pp: &Self::ProverParam,
        polys: impl IntoIterator<Item = &'a Self::Polynomial>,
        _: impl IntoIterator<Item = &'a Self::CommitmentWithAux>,
        points: &[Self::Point],
        evals: &[Evaluation<M::Scalar>],
        transcript: &mut impl TranscriptWrite<M::G1Affine, M::Scalar>,
    ) -> Result<(), Error> {
        let polys = polys.into_iter().collect_vec();
        validate_input("batch open", pp.num_vars(), polys.iter().copied(), points)?;

        let ell = evals.len().next_power_of_two().ilog2() as usize;
        let t = transcript.squeeze_challenges(ell);

        let timer = start_timer(|| "merged_polys");
        let eq_xt = MultilinearPolynomial::eq_xy(&t);
        let merged_polys = evals.iter().zip(eq_xt.evals().iter()).fold(
            vec![(M::Scalar::one(), Cow::<MultilinearPolynomial<_>>::default()); points.len()],
            |mut merged_polys, (eval, eq_xt_i)| {
                if merged_polys[eval.point()].1.is_zero() {
                    merged_polys[eval.point()] = (*eq_xt_i, Cow::Borrowed(polys[eval.poly()]));
                } else {
                    let coeff = merged_polys[eval.point()].0;
                    if coeff != M::Scalar::one() {
                        merged_polys[eval.point()].0 = M::Scalar::one();
                        *merged_polys[eval.point()].1.to_mut() *= &coeff;
                    }
                    *merged_polys[eval.point()].1.to_mut() += (eq_xt_i, polys[eval.poly()]);
                }
                merged_polys
            },
        );
        end_timer(timer);

        let expression = merged_polys
            .iter()
            .enumerate()
            .map(|(idx, (scalar, _))| {
                Expression::<M::Scalar>::eq_xy(idx)
                    * Expression::Polynomial(Query::new(idx, Rotation::cur()))
                    * scalar
            })
            .sum();
        let tilde_gs_sum =
            inner_product(evals.iter().map(Evaluation::value), &eq_xt[..evals.len()]);
        let (challenges, _) = ClassicSumCheck::<CoefficientsProver<_>>::prove(
            &(),
            pp.num_vars(),
            VirtualPolynomial::new(
                &expression,
                merged_polys.iter().map(|(_, poly)| poly.deref()),
                &[],
                points,
            ),
            tilde_gs_sum,
            transcript,
        )
        .unwrap();

        let timer = start_timer(|| "g_prime");
        let eq_xy_evals = points
            .iter()
            .map(|point| eq_xy_eval(&challenges, point))
            .collect_vec();
        let g_prime = merged_polys
            .into_iter()
            .zip(eq_xy_evals.iter())
            .map(|((scalar, poly), eq_xy_eval)| (scalar * eq_xy_eval, poly.into_owned()))
            .sum::<MultilinearPolynomial<_>>();
        end_timer(timer);

        let g_prime_eval = if cfg!(feature = "sanity-check") {
            g_prime.evaluate(&challenges)
        } else {
            M::Scalar::zero()
        };
        Self::open(
            pp,
            &g_prime,
            &MultilinearKzgCommitment::default(),
            &challenges,
            &g_prime_eval,
            transcript,
        )?;

        Ok(())
    }

    fn verify(
        vp: &Self::VerifierParam,
        comm: &Self::Commitment,
        point: &Self::Point,
        eval: &M::Scalar,
        transcript: &mut impl TranscriptRead<M::G1Affine, M::Scalar>,
    ) -> Result<(), Error> {
        validate_input("verify", vp.num_vars(), [], [point])?;

        let quotients = transcript.read_commitments(point.len())?;

        let window_size = window_size(point.len());
        let window_table = window_table(window_size, vp.g2);
        let rhs = iter::empty()
            .chain(Some(vp.g2.neg()))
            .chain(
                vp.ss(point.len())
                    .iter()
                    .cloned()
                    .zip_eq(fixed_base_msm(window_size, &window_table, point))
                    .map(|(s_i, x_i)| (s_i - x_i.into()).into()),
            )
            .map_into()
            .collect_vec();
        let lhs = iter::empty()
            .chain(Some((comm.to_curve() - vp.g1 * eval).into()))
            .chain(quotients.iter().cloned())
            .collect_vec();
        M::pairings_product_is_identity(&lhs.iter().zip_eq(rhs.iter()).collect_vec())
            .then_some(())
            .ok_or_else(|| Error::InvalidPcsOpen("Invalid multilinear KZG open".to_string()))
    }

    fn batch_verify(
        vp: &Self::VerifierParam,
        comms: &[Self::Commitment],
        points: &[Self::Point],
        evals: &[Evaluation<M::Scalar>],
        transcript: &mut impl TranscriptRead<M::G1Affine, M::Scalar>,
    ) -> Result<(), Error> {
        validate_input("batch verify", vp.num_vars(), [], points)?;

        let ell = evals.len().next_power_of_two().ilog2() as usize;
        let t = transcript.squeeze_challenges(ell);

        let eq_xt = MultilinearPolynomial::eq_xy(&t);
        let tilde_gs_sum =
            inner_product(evals.iter().map(Evaluation::value), &eq_xt[..evals.len()]);
        let (g_prime_eval, challenges) = ClassicSumCheck::<CoefficientsProver<_>>::verify(
            &(),
            vp.num_vars(),
            2,
            tilde_gs_sum,
            transcript,
        )?;
        let eq_xy_evals = points
            .iter()
            .map(|point| eq_xy_eval(&challenges, point))
            .collect_vec();
        let g_prime = variable_base_msm(
            &evals
                .iter()
                .zip(eq_xt.evals())
                .map(|(eval, eq_xt_i)| eq_xy_evals[eval.point()] * eq_xt_i)
                .collect_vec(),
            &evals.iter().map(|eval| comms[eval.poly()]).collect_vec(),
        )
        .into();
        Self::verify(vp, &g_prime, &challenges, &g_prime_eval, transcript)?;

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::{
        pcs::multilinear::{
            kzg::MultilinearKzg,
            test::{run_batch_commit_open_verify, run_commit_open_verify},
        },
        util::transcript::Keccak256Transcript,
    };
    use halo2_curves::bn256::{Bn256, Fr};

    #[test]
    fn commit_open_verify() {
        run_commit_open_verify::<Fr, MultilinearKzg<Bn256>, Keccak256Transcript<_>>();
    }

    #[test]
    fn batch_commit_open_verify() {
        run_batch_commit_open_verify::<Fr, MultilinearKzg<Bn256>, Keccak256Transcript<_>>();
    }
}
