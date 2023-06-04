use ark_ec::CurveGroup;
use ark_ec::Group;
use ark_ec::short_weierstrass::Projective;
use ark_ec::short_weierstrass::SWCurveConfig;
use binary::EcdsaInstance;
use ministark::utils::FieldVariant;
use num_bigint::BigUint;
use ruint::aliases::U256;
use ruint::uint;
use ark_ff::Field;
use crate::utils::gen_periodic_table;
use crate::utils::starkware_curve::Fr;
use crate::utils::starkware_curve::Curve;
use crate::utils::starkware_curve::calculate_slope;
use ministark_gpu::fields::p3618502788666131213697322783095070105623107215331596699973092056135872020481::ark::Fp;
use ark_ec::short_weierstrass::Affine;
use ark_ff::PrimeField;

pub const SHIFT_POINT: Affine<Curve> = super::pedersen::constants::P0;

#[derive(Clone, Debug)]
pub struct EcMultPartialStep {
    pub partial_sum: Affine<Curve>,
    pub fixed_point: Affine<Curve>,
    pub suffix: Fp,
    pub slope: Fp,
    pub x_diff_inv: Fp,
}

#[derive(Clone, Copy, Debug)]
pub struct DoublingStep {
    pub point: Affine<Curve>,
    pub slope: Fp,
}

#[derive(Clone, Debug)]
pub struct InstanceTrace {
    pub instance: EcdsaInstance,
    /// pubkey `Q`
    pub pubkey: Affine<Curve>,
    pub pubkey_doubling_steps: Vec<DoublingStep>,
    pub w: Fp,
    /// Inverse of `w` in the base field
    pub w_inv: Fp,
    pub r: Fp,
    /// Inverse of `r` in the base field
    pub r_inv: Fp,
    pub r_point_slope: Fp,
    pub r_point_x_diff_inv: Fp,
    /// Message hash `z`
    pub message: Fp,
    pub message_inv: Fp,
    /// Point `B = z * G + r * Q`
    pub b: Affine<Curve>,
    /// Slope between points `z * G` and `r * Q`
    pub b_slope: Fp,
    pub b_x_diff_inv: Fp,
    pub b_doubling_steps: Vec<DoublingStep>,
    /// steps for `z * G` where
    /// `G` is the elliptic curve generator point and
    /// `z` is the message hash
    pub zg_steps: Vec<EcMultPartialStep>,
    /// steps for the scalar multiplication `r * Q` where
    /// `Q` is the pubkey point and
    /// `r` is the signature's `r` value
    pub rq_steps: Vec<EcMultPartialStep>,
    /// steps for the scalar multiplication `w * B` where
    /// `B = z * G + r * Q` and
    /// `w` is the inverse of the signature's `s` value (NOTE: that's the
    /// inverse in the curve's scalar field)
    pub wb_steps: Vec<EcMultPartialStep>,
}

impl InstanceTrace {
    // TODO: error handling
    pub fn new(instance: EcdsaInstance) -> Self {
        let message = Fp::from(BigUint::from(instance.message));
        let pubkey_x = Fp::from(BigUint::from(instance.pubkey_x));
        let r = Fp::from(BigUint::from(instance.signature.r));
        let w = Fr::from(BigUint::from(instance.signature.w));
        let s = w.inverse().unwrap();
        let pubkey = verify(message, r, s, pubkey_x).expect("signature is invalid");

        let shift_point = Projective::from(SHIFT_POINT);
        let generator = Projective::from(Curve::GENERATOR);

        let zg = Affine::from(mimic_ec_mult_air(message.into(), generator, -shift_point).unwrap());
        let qr = Affine::from(mimic_ec_mult_air(r.into(), pubkey.into(), shift_point).unwrap());

        let b = (zg + qr).into_affine();
        let b_slope = calculate_slope(zg, qr).unwrap();
        let b_x_diff_inv = (zg.x - qr.x).inverse().unwrap();
        let b_doubling_steps = doubling_steps(b.into());
        let wb = Affine::from(mimic_ec_mult_air(w.into(), b.into(), shift_point).unwrap());

        let zg_steps = gen_ec_mult_steps(message.into(), generator, -shift_point);
        let rq_steps = gen_ec_mult_steps(r.into(), pubkey.into(), shift_point);
        let wb_steps = gen_ec_mult_steps(w.into(), b.into(), shift_point);

        assert_eq!(zg, zg_steps.last().unwrap().partial_sum);
        assert_eq!(qr, rq_steps.last().unwrap().partial_sum);
        assert_eq!(wb, wb_steps.last().unwrap().partial_sum);

        let w = Fp::from(BigUint::from(w));
        let w_inv = w.inverse().unwrap();
        let r_inv = r.inverse().unwrap();
        let message_inv = message.inverse().unwrap();

        let pubkey_doubling_steps = doubling_steps(pubkey.into());

        let shift_point = Affine::from(shift_point);
        let r_point_slope = calculate_slope(wb, -shift_point).unwrap();
        let r_point_x_diff_inv = (wb.x - (-shift_point).x).inverse().unwrap();
        assert_eq!(r, (wb - shift_point).into_affine().x);

        Self {
            instance,
            pubkey,
            pubkey_doubling_steps,
            w,
            w_inv,
            r,
            r_inv,
            r_point_slope,
            r_point_x_diff_inv,
            message,
            message_inv,
            b,
            b_slope,
            b_x_diff_inv,
            b_doubling_steps,
            zg_steps,
            rq_steps,
            wb_steps,
        }
    }
}

/// Generates a list of the steps involved with an elliptic curve multiply
fn gen_ec_mult_steps(
    x: BigUint,
    mut point: Projective<Curve>,
    shift_point: Projective<Curve>,
) -> Vec<EcMultPartialStep> {
    let x = U256::from(x);
    // Assertions fail if the AIR will error
    assert!(x != U256::ZERO);
    assert!(x < uint!(2_U256).pow(uint!(251_U256)));
    let mut partial_sum = shift_point;
    let mut res = Vec::new();
    for i in 0..256 {
        let suffix = x >> i;
        let bit = suffix & uint!(1_U256);

        let mut slope: Fp = Fp::ZERO;
        let mut partial_sum_next = partial_sum;
        let partial_sum_affine = partial_sum.into_affine();
        let point_affine = point.into_affine();
        if bit == uint!(1_U256) {
            slope = calculate_slope(point_affine, partial_sum_affine).unwrap();
            partial_sum_next += point;
        }

        res.push(EcMultPartialStep {
            partial_sum: partial_sum_affine,
            fixed_point: point_affine,
            suffix: Fp::from(BigUint::from(suffix)),
            x_diff_inv: (partial_sum_affine.x - point_affine.x).inverse().unwrap(),
            slope,
        });

        partial_sum = partial_sum_next;
        point.double_in_place();
    }
    res
}

fn doubling_steps(mut p: Projective<Curve>) -> Vec<DoublingStep> {
    let mut res = Vec::new();
    #[allow(clippy::needless_range_loop)]
    for _ in 0..256 {
        let p_affine = p.into_affine();
        let slope = calculate_slope(p_affine, p_affine).unwrap();
        res.push(DoublingStep {
            point: p_affine,
            slope,
        });
        p.double_in_place();
    }
    res
}

/// Verifies a signature
/// Returns the associated public key if the signature is valid
/// Returns None if the signature is invalid
/// based on: https://github.com/starkware-libs/starkex-resources/blob/844ac3dcb1f735451457f7eecc6e37cd96d1cb2d/crypto/starkware/crypto/signature/signature.py#L192
fn verify(msg_hash: Fp, r: Fp, s: Fr, pubkey_x: Fp) -> Option<Affine<Curve>> {
    let w = s.inverse().unwrap();
    let (y1, y0) = Affine::<Curve>::get_ys_from_x_unchecked(pubkey_x).expect("not on the curve");

    for pubkey_y in [y1, y0] {
        let pubkey = Affine::<Curve>::new_unchecked(pubkey_x, pubkey_y);
        // Signature validation.
        // DIFF: original formula is:
        // x = (w*msg_hash)*EC_GEN + (w*r)*public_key
        // While what we implement is:
        // x = w*(msg_hash*EC_GEN + r*public_key).
        // While both mathematically equivalent, one might error while the other
        // doesn't, given the current implementation.
        // This formula ensures that if the verification errors in our AIR, it
        // errors here as well.
        let shift_point = Projective::from(SHIFT_POINT);
        let generator = Curve::GENERATOR.into();
        let zg = mimic_ec_mult_air(msg_hash.into(), generator, -shift_point).unwrap();
        let rq = mimic_ec_mult_air(r.into(), pubkey.into(), shift_point).unwrap();
        let wb = mimic_ec_mult_air(w.into(), zg + rq, shift_point).unwrap();
        let x = (wb - shift_point).into_affine().x;
        if r == x {
            return Some(pubkey);
        }
    }

    None
}

/// Computes `m * point + shift_point` using the same steps like the AIR and
/// Returns None if and only if the AIR errors.
fn mimic_ec_mult_air(
    m: BigUint,
    mut point: Projective<Curve>,
    shift_point: Projective<Curve>,
) -> Option<Projective<Curve>> {
    println!("{}", Fp::MODULUS_BIT_SIZE);
    if !(1..Fp::MODULUS_BIT_SIZE).contains(&(m.bits() as u32)) {
        return None;
    }
    let mut m = U256::from(m);
    let mut partial_sum = shift_point;
    #[allow(clippy::needless_range_loop)]
    while m != U256::ZERO {
        if Affine::from(partial_sum).x == Affine::from(point).x {
            return None;
        }
        let bit = m & uint!(1_U256);
        if bit == uint!(1_U256) {
            partial_sum += point;
        }
        point.double_in_place();
        m >>= 1;
    }
    Some(partial_sum)
}

/// Ouptut is of the form (x_points_coeffs, y_points_coeffs)
// TODO: Generate these constant polynomials at compile time
#[allow(clippy::type_complexity)]
pub fn generator_points_poly() -> (Vec<FieldVariant<Fp, Fp>>, Vec<FieldVariant<Fp, Fp>>) {
    let mut evals = Vec::new();

    let mut acc = Projective::from(Curve::GENERATOR);
    for _ in 0..256 {
        let p = acc.into_affine();
        evals.push((p.x, p.y));
        acc.double_in_place();
    }

    // TODO: need to figure out the exact polynomial starkware is using
    // assert_eq!(evals.len(), 256 + 252);
    // evals.resize(512, (Fp::ZERO, Fp::ZERO));

    let (x_evals, y_evals) = evals.into_iter().unzip();
    let mut polys = gen_periodic_table(vec![x_evals, y_evals])
        .into_iter()
        .map(|poly| poly.coeffs.into_iter().map(FieldVariant::Fp).collect());
    let (x_coeffs, y_coeffs) = (polys.next().unwrap(), polys.next().unwrap());
    (x_coeffs, y_coeffs)
}
