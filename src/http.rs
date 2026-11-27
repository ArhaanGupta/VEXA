//! Shared HTTP client setup. On Windows, TLS uses the OS stack and certificate store.

use std::time::Duration;

pub fn agent(timeout: Duration) -> ureq::Agent {
    let builder = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false);
    #[cfg(windows)]
    let builder = builder.tls_config(
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build(),
    );
    builder.build().into()
}
