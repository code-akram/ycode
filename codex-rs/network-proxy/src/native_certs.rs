use rama_tls_rustls::dep::pki_types::CertificateDer;
use rustls_native_certs::CertificateResult;
use rustls_native_certs::Error;
use rustls_native_certs::ErrorKind;

// `rustls_native_certs::load_native_certs()` first consults SSL_CERT_FILE and
// SSL_CERT_DIR. Load platform roots directly so a startup custom CA can be
// layered onto the managed bundle without replacing the platform trust store.
pub(crate) fn load_platform_native_certs() -> CertificateResult {
    use security_framework::trust_settings::Domain;
    use security_framework::trust_settings::TrustSettings;
    use security_framework::trust_settings::TrustSettingsForCertificate;
    use std::collections::BTreeMap;

    let mut result = CertificateResult::default();
    let mut all_certs = BTreeMap::new();
    for domain in &[Domain::User, Domain::Admin, Domain::System] {
        let ts = TrustSettings::new(*domain);
        let iter = match ts.iter() {
            Ok(iter) => iter,
            Err(err) => {
                result.errors.push(Error {
                    context: match domain {
                        Domain::User => "failed to load user trust settings",
                        Domain::Admin => "failed to load admin trust settings",
                        Domain::System => "failed to load system trust settings",
                    },
                    kind: ErrorKind::Os(err.into()),
                });
                continue;
            }
        };

        for cert in iter {
            let der = cert.to_der();
            let trusted = match ts.tls_trust_settings_for_certificate(&cert) {
                Ok(trusted) => trusted.unwrap_or(TrustSettingsForCertificate::TrustRoot),
                Err(err) => {
                    result.errors.push(Error {
                        context: "certificate not trusted",
                        kind: ErrorKind::Os(err.into()),
                    });
                    continue;
                }
            };
            all_certs.entry(der).or_insert(trusted);
        }
    }

    for (der, trusted) in all_certs {
        use TrustSettingsForCertificate::*;

        if let TrustRoot | TrustAsRoot = trusted {
            result.certs.push(CertificateDer::from(der));
        }
    }
    result
}
