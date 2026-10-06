//! The media region `rooms.join` is asked for: the one nearest to us.
//!
//! AWS answers that at `nearest-media-region.l.chime.aws`, as the Chime
//! SDK's demo asks it (`getNearestMediaRegion` in
//! `demos/browser/app/meetingV2/meetingV2.ts`: a GET whose JSON names it in
//! `region`). It is AWS's, not Slack's, so it goes without any Slack token
//! or cookie, through the app's proxy setting, and may take two seconds at
//! most. A region the probe was given wins; one that could not be found
//! falls back to [`super::join::DEFAULT_REGION`]. A region found is kept
//! for the rest of the run; a failed lookup is tried again next time.

use std::sync::Mutex;
use std::time::Duration;

/// Where AWS says which media region is nearest.
pub const NEAREST_URL: &str = "https://nearest-media-region.l.chime.aws";
/// How long the lookup may take before the fallback is used.
const TIMEOUT: Duration = Duration::from_secs(2);
/// The most of an answer read: it is a few dozen bytes.
const MAX_ANSWER: usize = 1024;

/// The region found this run.
static FOUND: Mutex<Option<String>> = Mutex::new(None);

/// Whether `region` looks like an AWS region name (`eu-central-1`):
/// lower-case letters, digits and hyphens, starting with a letter, not
/// ending with a hyphen, at most 32 characters. Anything else is not put
/// in a request.
pub fn valid(region: &str) -> bool {
    (2..=32).contains(&region.len())
        && region.starts_with(|c: char| c.is_ascii_lowercase())
        && !region.ends_with('-')
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The region in an answer of `{"region": "…"}`, if it is one.
pub fn parse(answer: &[u8]) -> Option<String> {
    if answer.len() > MAX_ANSWER {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(answer).ok()?;
    let region = value.get("region")?.as_str()?;
    valid(region).then(|| region.to_owned())
}

/// Why a region was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// It was asked for (`--huddle-region`).
    Asked,
    /// AWS said it is the nearest.
    Nearest,
    /// Neither: the fallback.
    Fallback,
}

/// The region to use: the one asked for if it is valid, else the nearest
/// found, else the fallback.
pub fn choose(asked: Option<&str>, nearest: Option<&str>) -> (String, Why) {
    if let Some(asked) = asked.filter(|r| valid(r)) {
        return (asked.to_owned(), Why::Asked);
    }
    if let Some(nearest) = nearest.filter(|r| valid(r)) {
        return (nearest.to_owned(), Why::Nearest);
    }
    (super::join::DEFAULT_REGION.to_owned(), Why::Fallback)
}

/// Asks AWS for the nearest region, once per run while it answers.
pub async fn nearest() -> Option<String> {
    if let Some(found) = lock().clone() {
        return Some(found);
    }
    let found = ask().await;
    if let Some(region) = &found {
        *lock() = Some(region.clone());
    }
    found
}

fn lock() -> std::sync::MutexGuard<'static, Option<String>> {
    FOUND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One lookup, logged when it fails.
async fn ask() -> Option<String> {
    // The app's client sends no Slack credentials by itself: they are
    // added per Slack request, and this is not one.
    let sent = crate::slack::net::api()
        .get(NEAREST_URL)
        .timeout(TIMEOUT)
        .send()
        .await;
    let response = match sent {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            log::info!(
                "region: the nearest-region lookup answered HTTP {}",
                response.status()
            );
            return None;
        }
        Err(error) => {
            log::info!(
                "region: the nearest-region lookup failed: {}",
                error.without_url()
            );
            return None;
        }
    };
    match response.bytes().await {
        Ok(bytes) => {
            let region = parse(&bytes);
            if region.is_none() {
                log::info!("region: the nearest-region answer names no region");
            }
            region
        }
        Err(error) => {
            log::info!(
                "region: the nearest-region answer did not arrive: {}",
                error.without_url()
            );
            None
        }
    }
}

/// The region for `rooms.join`, logging which and why.
pub async fn for_join(asked: Option<&str>) -> String {
    if let Some(asked) = asked.filter(|r| !valid(r)) {
        log::warn!("region: {asked:?} is not a region name; ignoring it");
    }
    let nearest = if asked.is_some_and(valid) {
        None
    } else {
        nearest().await
    };
    let (region, why) = choose(asked, nearest.as_deref());
    match why {
        Why::Asked => log::info!("region: {region}, as asked"),
        Why::Nearest => log::info!(
            "region: {region}, the nearest by {}",
            super::host_of(NEAREST_URL)
        ),
        Why::Fallback => log::info!("region: {region}, the fallback (the nearest is not known)"),
    }
    region
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_names_are_checked() {
        for good in [
            "us-east-1",
            "eu-central-1",
            "ap-southeast-2",
            "us-gov-west-1",
        ] {
            assert!(valid(good), "{good}");
        }
        for bad in [
            "",
            "u",
            "US-EAST-1",
            "us east 1",
            "-us-east-1",
            "us-east-1-",
            "1us-east",
            "us-east-1&x=1",
            "eu-central-1\n",
            "a-very-long-region-name-that-goes-on",
        ] {
            assert!(!valid(bad), "{bad:?}");
        }
    }

    #[test]
    fn answers_are_read_only_when_they_name_a_region() {
        assert_eq!(
            parse(br#"{"region":"eu-central-1"}"#),
            Some("eu-central-1".into())
        );
        assert_eq!(
            parse(br#"{ "region": "us-west-2", "other": 1 }"#),
            Some("us-west-2".into())
        );
        assert_eq!(parse(b"eu-central-1"), None, "not JSON");
        assert_eq!(parse(b"<html>hi</html>"), None);
        assert_eq!(parse(br#"{"Region":"eu-central-1"}"#), None, "another key");
        assert_eq!(parse(br#"{"region":1}"#), None, "not a string");
        assert_eq!(parse(br#"["eu-central-1"]"#), None, "not an object");
        assert_eq!(parse(br#"{"region":"EU; drop"}"#), None, "not a name");
        let long = format!(
            r#"{{"region":"eu-central-1","pad":"{}"}}"#,
            "x".repeat(2000)
        );
        assert_eq!(parse(long.as_bytes()), None, "too long");
    }

    #[test]
    fn asked_beats_nearest_beats_the_fallback() {
        assert_eq!(
            choose(Some("ap-northeast-1"), Some("eu-central-1")),
            ("ap-northeast-1".into(), Why::Asked)
        );
        assert_eq!(
            choose(None, Some("eu-central-1")),
            ("eu-central-1".into(), Why::Nearest)
        );
        assert_eq!(
            choose(Some("Not A Region"), Some("eu-central-1")),
            ("eu-central-1".into(), Why::Nearest)
        );
        assert_eq!(choose(None, None), ("us-east-1".into(), Why::Fallback));
        assert_eq!(
            choose(None, Some("junk!")),
            ("us-east-1".into(), Why::Fallback)
        );
    }
}
