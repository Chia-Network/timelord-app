use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

pub(crate) fn ca(name: &str) -> (Issuer<'static, KeyPair>, Vec<u8>, Vec<u8>) {
    let mut params = CertificateParams::new(vec![name.into()]).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    let cert_pem = certificate.pem().into_bytes();
    let key_pem = key.serialize_pem().into_bytes();
    (Issuer::new(params, key), cert_pem, key_pem)
}

/// An intermediate CA issued by `issuer`, for exercising chain files.
pub(crate) fn intermediate(
    issuer: &Issuer<'_, KeyPair>,
    name: &str,
) -> (Issuer<'static, KeyPair>, Vec<u8>) {
    let mut params = CertificateParams::new(vec![name.into()]).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    let key = KeyPair::generate().unwrap();
    let certificate = params.clone().signed_by(&key, issuer).unwrap();
    (Issuer::new(params, key), certificate.pem().into_bytes())
}

pub(crate) fn leaf(issuer: &Issuer<'_, KeyPair>, sans: &[&str]) -> (Vec<u8>, Vec<u8>) {
    leaf_with_key(issuer, &KeyPair::generate().unwrap(), sans)
}

/// A certificate issued by `issuer` that presents `key` as its subject key.
pub(crate) fn leaf_with_key(
    issuer: &Issuer<'_, KeyPair>,
    key: &KeyPair,
    sans: &[&str],
) -> (Vec<u8>, Vec<u8>) {
    let sans = sans.iter().map(|san| (*san).to_owned()).collect::<Vec<_>>();
    let certificate = CertificateParams::new(sans)
        .unwrap()
        .signed_by(key, issuer)
        .unwrap();
    (
        certificate.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    )
}
