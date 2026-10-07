//! Timestamps as commits and operations record them: Unix seconds plus the
//! local UTC offset at that moment, written `1759737600 +0530` (spec §4).

use std::fmt;

use chrono::{DateTime, FixedOffset, Local, Offset as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    pub secs: i64,
    /// Minutes east of UTC.
    pub offset_minutes: i32,
}

impl Timestamp {
    /// The current time, with this machine's current UTC offset.
    pub fn now() -> Self {
        let now = Local::now();
        Self {
            secs: now.timestamp(),
            offset_minutes: now.offset().fix().local_minus_utc() / 60,
        }
    }

    /// Parses the canonical form, `<secs> <+|-><HHMM>`. Anything that wouldn't
    /// print back identically is rejected.
    pub fn parse(text: &str) -> Option<Self> {
        let (secs_text, offset) = text.split_once(' ')?;
        let secs: i64 = secs_text.parse().ok()?;
        if secs.to_string() != secs_text {
            return None;
        }
        let offset = offset.as_bytes();
        if offset.len() != 5 || !offset[1..].iter().all(u8::is_ascii_digit) {
            return None;
        }
        let sign = match offset[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let digit = |i: usize| i32::from(offset[i] - b'0');
        let hours = digit(1) * 10 + digit(2);
        let minutes = digit(3) * 10 + digit(4);
        // A zero offset is always written `+0000`, so `-0000` isn't canonical.
        if minutes >= 60 || (sign < 0 && hours == 0 && minutes == 0) {
            return None;
        }
        Some(Self {
            secs,
            offset_minutes: sign * (hours * 60 + minutes),
        })
    }

    /// For people, in the recorded offset: `Mon Oct 6 15:20:00 2026 +0530`.
    pub fn display(&self) -> String {
        FixedOffset::east_opt(self.offset_minutes * 60)
            .zip(DateTime::from_timestamp(self.secs, 0))
            .map_or_else(
                || self.to_string(),
                |(offset, time)| {
                    time.with_timezone(&offset)
                        .format("%a %b %-d %H:%M:%S %Y %z")
                        .to_string()
                },
            )
    }
}

/// The canonical form, as stored in objects.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.offset_minutes < 0 { '-' } else { '+' };
        let minutes = self.offset_minutes.unsigned_abs();
        write!(
            f,
            "{} {sign}{:02}{:02}",
            self.secs,
            minutes / 60,
            minutes % 60
        )
    }
}

/// Where "now" comes from. Tests and scripts pin it so hashes are reproducible.
#[derive(Clone, Copy, Debug)]
pub enum Clock {
    System,
    Fixed(Timestamp),
}

impl Clock {
    pub fn now(self) -> Timestamp {
        match self {
            Self::System => Timestamp::now(),
            Self::Fixed(time) => time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_round_trip() {
        for text in [
            "1759737600 +0530",
            "0 +0000",
            "-86400 -0800",
            "1759737600 +1400",
        ] {
            let time = Timestamp::parse(text).unwrap();
            assert_eq!(time.to_string(), text);
        }
    }

    #[test]
    fn rejects_non_canonical_forms() {
        for text in [
            "+1759737600 +0530",
            "01759737600 +0530",
            "1759737600 0530",
            "1759737600 +530",
            "1759737600 +0560",
            "1759737600  +0530",
            "1759737600 -0000",
            "abc +0000",
        ] {
            assert_eq!(Timestamp::parse(text), None, "{text}");
        }
    }

    #[test]
    fn displays_in_the_recorded_offset() {
        let time = Timestamp::parse("1759737600 +0530").unwrap();
        assert_eq!(time.display(), "Mon Oct 6 13:30:00 2025 +0530");
        let time = Timestamp::parse("1759737600 -0800").unwrap();
        assert_eq!(time.display(), "Mon Oct 6 00:00:00 2025 -0800");
    }
}
