//! Shared HTTP client setup. On Windows, TLS uses the OS stack and certificate store.

use std::time::Duration;

pub fn agent(timeout: Duration) -> ureq::Agent {
    agent_with(timeout, 10)
}

/// Like `agent`, but with a redirect limit (0 lets the caller check each redirect itself).
pub fn agent_with(timeout: Duration, max_redirects: u32) -> ureq::Agent {
    let builder = ureq::Agent::config_builder()
        .max_redirects(max_redirects)
        .max_redirects_will_error(false)
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
