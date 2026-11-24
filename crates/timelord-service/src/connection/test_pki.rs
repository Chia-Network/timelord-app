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

pub(crate) fn leaf(issuer: &Issuer<'_, KeyPair>, sans: &[&str]) -> (Vec<u8>, Vec<u8>) {
    let key = KeyPair::generate().unwrap();
    let sans = sans.iter().map(|san| (*san).to_owned()).collect::<Vec<_>>();
    let certificate = CertificateParams::new(sans)
        .unwrap()
        .signed_by(&key, issuer)
        .unwrap();
    (
        certificate.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    )
}
