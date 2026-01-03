use std::time::Duration;

use reqwest::{Client, header::HeaderMap};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};

pub fn http_client(default_headers: HeaderMap) -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .default_headers(default_headers)
        .build()
        .unwrap();

    ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}
