//! Wire encodings pinned by design §6.7.
//!
//! serde's defaults are not what a model or a replayer should see, so the crossing types carry
//! their own representations:
//!
//! * `Duration` → integer **seconds** where it is a *bound*, integer **milliseconds** where it is
//!   a *measurement*. Bounds round **up**, measurements round **down**, so a bound is never
//!   silently shortened and a measurement never claims time it did not take.
//! * `SystemTime` → RFC3339 in UTC with a literal `Z` and exactly three fractional digits,
//!   sub-millisecond precision **truncated, never rounded** — a timestamp must not round forward
//!   past an event that followed it.

use std::time::{Duration as StdDuration, SystemTime as StdSystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A bound, serialized as integer seconds rounded **up**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration(pub StdDuration);

impl Duration {
    pub fn from_secs(s: u64) -> Self {
        Self(StdDuration::from_secs(s))
    }

    /// Rounds up: a 1500 ms bound serializes as 2 s, never 1.
    fn as_secs_ceil(&self) -> u64 {
        let d = self.0;
        d.as_secs() + u64::from(d.subsec_nanos() > 0)
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.as_secs_ceil())
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(StdDuration::from_secs(u64::deserialize(d)?)))
    }
}

/// A measurement, serialized as integer milliseconds rounded **down**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Millis(pub StdDuration);

impl Serialize for Millis {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u128(self.0.as_millis())
    }
}

impl<'de> Deserialize<'de> for Millis {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(StdDuration::from_millis(u64::deserialize(d)?)))
    }
}

/// RFC3339, UTC, literal `Z`, exactly three fractional digits, truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTime(pub StdSystemTime);

impl SystemTime {
    pub fn from_unix_millis(ms: u64) -> Self {
        Self(UNIX_EPOCH + StdDuration::from_millis(ms))
    }

    fn to_rfc3339_z(self) -> String {
        let d = self.0.duration_since(UNIX_EPOCH).unwrap_or_default();
        let secs = d.as_secs() as i64;
        // truncate, never round
        let millis = d.subsec_millis();
        let (y, mo, dd, hh, mi, ss) = civil_from_unix(secs);
        format!("{y:04}-{mo:02}-{dd:02}T{hh:02}:{mi:02}:{ss:02}.{millis:03}Z")
    }
}

/// days-from-civil, per Howard Hinnant's algorithm — no chrono dependency in `marion-core`.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

impl Serialize for SystemTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_rfc3339_z())
    }
}

impl<'de> Deserialize<'de> for SystemTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_rfc3339_z(&s).ok_or_else(|| serde::de::Error::custom(format!("bad RFC3339: {s}")))
    }
}

fn parse_rfc3339_z(s: &str) -> Option<SystemTime> {
    // YYYY-MM-DDTHH:MM:SS.mmmZ
    if s.len() != 24 || !s.ends_with('Z') {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let ms = num(20, 23)?;
    let days = unix_from_civil(y, mo as u32, d as u32);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    Some(SystemTime::from_unix_millis((secs * 1000 + ms) as u64))
}

fn unix_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// What every line decoder in this crate owes a reader: `None`, never a guess and never a panic,
/// for an empty line, a torn prefix (the writer died mid-record), a line that is not JSON at all,
/// and bytes that are not UTF-8. `torn_prefix` is the caller's own record shape cut short, so the
/// assertion exercises that codec's fields and not a generic one.
#[cfg(test)]
pub(crate) fn assert_decode_rejects_rather_than_guesses<T: std::fmt::Debug>(
    decode: fn(&[u8]) -> Option<T>,
    torn_prefix: &[u8],
) {
    assert!(decode(b"").is_none(), "an empty line");
    assert!(decode(torn_prefix).is_none(), "a torn prefix");
    assert!(decode(b"not json").is_none(), "a line that is not JSON");
    assert!(
        decode(&[0xff, 0xfe]).is_none(),
        "invalid UTF-8 must not panic"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_round_up_measurements_round_down() {
        let bound = Duration(StdDuration::from_millis(1500));
        assert_eq!(
            serde_json::to_string(&bound).unwrap(),
            "2",
            "a bound must never shorten"
        );
        let measure = Millis(StdDuration::from_micros(1500));
        assert_eq!(
            serde_json::to_string(&measure).unwrap(),
            "1",
            "a measurement must not inflate"
        );
    }

    #[test]
    fn sub_millisecond_truncates_never_rounds() {
        // 999_999 ns is 0.999 ms; rounding would push it to the next millisecond and could
        // order this timestamp after an event that actually followed it.
        let t = SystemTime(UNIX_EPOCH + StdDuration::from_nanos(1_999_999));
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            "\"1970-01-01T00:00:00.001Z\""
        );
    }

    #[test]
    fn timestamps_round_trip() {
        let t = SystemTime::from_unix_millis(1_785_625_628_619);
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(s, "\"2026-08-01T23:07:08.619Z\"");
        let back: SystemTime = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
    }
}
