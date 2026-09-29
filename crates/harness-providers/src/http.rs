//! The HTTP clients harness talks to providers and the sign-in server with.

use reqwest::redirect::{Attempt, Policy};

/// How many redirects a request follows at most, as reqwest's default policy.
const MAX_REDIRECTS: usize = 10;

/// A client builder whose requests follow a redirect only within their origin (scheme, host and
/// port): an API key or token sent in a header (`x-api-key`, which reqwest does not strip) or in a
/// body (which a 307 or 308 re-sends) never reaches another host. A redirect to another origin is
/// refused with an error that names both.
pub fn client() -> reqwest::ClientBuilder {
    reqwest::Client::builder().redirect(Policy::custom(same_origin))
}

fn same_origin(attempt: Attempt) -> reqwest::redirect::Action {
    // The request's own URL comes first.
    let Some(from) = attempt.previous().first().map(|url| url.origin()) else {
        return attempt.follow();
    };
    let to = attempt.url().origin();
    if from != to {
        let refused = format!(
            "harness does not follow a redirect to another origin, from {} to {}",
            from.ascii_serialization(),
            to.ascii_serialization()
        );
        attempt.error(refused)
    } else if attempt.previous().len() > MAX_REDIRECTS {
        attempt.error("too many redirects")
    } else {
        attempt.follow()
    }
}

/// `error`, with its causes, which reqwest's own message leaves out ("connection refused", or a
/// redirect refused).
pub fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(error) = cause {
        let said = error.to_string();
        if !text.contains(&said) {
            text.push_str(": ");
            text.push_str(&said);
        }
        cause = error.source();
    }
    text
}
