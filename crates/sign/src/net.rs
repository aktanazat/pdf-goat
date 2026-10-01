//! The network boundary of `security sign --tsa`/`--ltv` and `security verify --online`:
//! one HTTP(S) agent whose timeout bounds every request.

use std::time::Duration;

/// The largest answer read: a time-stamp, OCSP response, CRL, or certificate.
const MAX_BODY: u64 = 32 * 1024 * 1024;

pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    pub fn new(timeout: Duration) -> Http {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .into();
        Http { agent }
    }

    /// The body of the answer to `body` POSTed as `content_type`.
    pub fn post(&self, url: &str, content_type: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent
            .post(url)
            .header("Content-Type", content_type)
            .send(body)
            .map_err(|e| format!("{url}: {e}"))?;
        read_body(url, &mut response)
    }

    /// The body of the answer to a GET of `url`.
    pub fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        read_body(url, &mut response)
    }
}

fn read_body(
    url: &str,
    response: &mut ureq::http::Response<ureq::Body>,
) -> Result<Vec<u8>, String> {
    response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| format!("{url}: {e}"))
}
