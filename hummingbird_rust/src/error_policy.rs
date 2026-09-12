//! HTTP status classification for venue REST calls.
//!
//! 429 (and transport `0`) are retried with backoff. Other 4xx/5xx abort the
//! pair and send `AbortFatal` to the peer.

pub fn http_is_rate_limited(status: u16) -> bool {
    status == 429
}

/// Fatal for arb coordination: anything except 429 (after retries exhausted) and success.
pub fn http_is_fatal(status: u16) -> bool {
    status != 429 && status >= 400
}

pub fn http_should_retry(status: u16, _body: &str) -> bool {
    status == 429 || status == 0
}
