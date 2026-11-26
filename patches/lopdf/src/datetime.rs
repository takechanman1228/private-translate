use super::Object;

#[cfg(feature = "chrono")]
mod chrono_impl {
    use crate::Object;
    use crate::datetime::{date_literal, date_with_offset};
    use chrono::prelude::*;

    impl From<DateTime<FixedOffset>> for Object {
        fn from(date: DateTime<FixedOffset>) -> Self {
            date_with_offset(date.format("D:%Y%m%d%H%M%S%:z'").to_string())
        }
    }

    impl From<DateTime<Utc>> for Object {
        fn from(date: DateTime<Utc>) -> Self {
            date_literal(date.format("D:%Y%m%d%H%M%SZ").to_string())
        }
    }

    /// A PDF date states a fixed offset from UT and never a named zone
    /// (ISO 32000-1, 7.9.4), so this is the conversion that keeps what the
    /// file said. Re-expressing it in the machine's own zone is what the
    /// `chrono-clock` feature adds.
    impl TryFrom<super::DateTime> for DateTime<FixedOffset> {
        type Error = chrono::format::ParseError;

        fn try_from(value: super::DateTime) -> Result<DateTime<FixedOffset>, Self::Error> {
            let from_date = |date: NaiveDate| {
                FixedOffset::east_opt(0)
                    .unwrap()
                    .from_utc_datetime(&date.and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap()))
            };

            DateTime::parse_from_str(&value.0, "%Y%m%d%H%M%S%#z")
                .or_else(|_| DateTime::parse_from_str(&value.0, "%Y%m%d%H%M%#z"))
                .or_else(|_| NaiveDate::parse_from_str(&value.0, "%Y%m%d").map(from_date))
        }
    }
}

/// The system-local conversions, kept apart from `chrono` because `Local`
/// exists only with chrono's `clock` feature — which brings `iana-time-zone`
/// and its per-platform chain. A PDF date never names a zone, so a consumer
/// that does not want the reading machine's offset applied to someone else's
/// document can have dates for `chrono` and `num-traits` alone.
#[cfg(feature = "chrono-clock")]
mod chrono_clock_impl {
    use crate::Object;
    use crate::datetime::date_with_offset;
    use chrono::prelude::*;

    impl From<DateTime<Local>> for Object {
        fn from(date: DateTime<Local>) -> Self {
            date_with_offset(date.format("D:%Y%m%d%H%M%S%:z'").to_string())
        }
    }

    impl TryFrom<super::DateTime> for DateTime<Local> {
        type Error = chrono::format::ParseError;

        fn try_from(value: super::DateTime) -> Result<DateTime<Local>, Self::Error> {
            DateTime::<FixedOffset>::try_from(value).map(|date| date.with_timezone(&Local))
        }
    }
}

#[cfg(feature = "jiff")]
mod jiff_impl {
    use crate::Object;
    use crate::datetime::{date_literal, date_with_offset};
    use jiff::{Timestamp, Zoned};

    impl From<Zoned> for Object {
        fn from(date: Zoned) -> Self {
            date_with_offset(date.strftime("D:%Y%m%d%H%M%S%:z'").to_string())
        }
    }

    impl From<Timestamp> for Object {
        fn from(date: Timestamp) -> Self {
            date_literal(date.strftime("D:%Y%m%d%H%M%SZ").to_string())
        }
    }

    impl TryFrom<super::DateTime> for Zoned {
        type Error = jiff::Error;

        fn try_from(value: super::DateTime) -> Result<Self, Self::Error> {
            use jiff::civil::{Date, DateTime};

            // Date formats from PDF 32000-1:2008 §7.9.4. Per the spec, a plus/minus O field
            // means local time is later/earlier than UT, a `Z` means equal to UT, and a missing
            // offset means GMT. Formats are tried in order, each more lenient than the last,
            // and a `Zoned` is always returned to preserve the timezone.
            Zoned::strptime("%Y%m%d%H%M%S%#z", &value.0)
                .or_else(|_| DateTime::strptime("%Y%m%d%H%M%SZ", &value.0).and_then(|dt| dt.in_tz("UTC")))
                .or_else(|_| Zoned::strptime("%Y%m%d%H%M%#z", &value.0))
                .or_else(|_| DateTime::strptime("%Y%m%d%H%MZ", &value.0).and_then(|dt| dt.in_tz("UTC")))
                .or_else(|_| Date::strptime("%Y%m%d", &value.0).and_then(|dt| dt.at(0, 0, 0, 0).in_tz("GMT")))
        }
    }
}

#[cfg(feature = "time")]
mod time_impl {
    use super::date_literal;
    use crate::Object;
    use time::{OffsetDateTime, PrimitiveDateTime};

    /// The naive datetime is taken to be UTC and rendered with the `Z` suffix
    /// (PDF 32000-1 §7.9.4) — the `time`-crate counterpart of the
    /// `chrono::DateTime<Utc>` and `jiff::Timestamp` impls above.
    ///
    /// (This replaces a `From<time::Time>` impl that never compiled — see
    /// issue #518: it referenced a nonexistent `FormatItem::StringLiteral`
    /// with a strftime pattern, and a bare time-of-day cannot form a valid
    /// PDF date anyway, which must start at the year.)
    impl From<PrimitiveDateTime> for Object {
        fn from(date: PrimitiveDateTime) -> Self {
            // D:%Y%m%d%H%M%SZ
            let format =
                time::format_description::parse_borrowed::<3>("D:[year][month][day][hour][minute][second]Z").unwrap();
            date_literal(date.format(&format).unwrap())
        }
    }

    impl From<OffsetDateTime> for Object {
        fn from(date: OffsetDateTime) -> Self {
            // The `time` crate spells PDF's offset separator directly (the
            // literal `'` in the format), so unlike chrono/jiff there is no
            // `:` to fix up and the bytes are already correct.
            let format = time::format_description::parse_borrowed::<3>(
                "D:[year][month][day][hour][minute][second][offset_hour sign:mandatory]'[offset_minute]'",
            )
            .unwrap();
            date_literal(date.format(&format).unwrap())
        }
    }

    /// WARNING: `tm_wday` (weekday), `tm_yday` (day index in year), `tm_isdst`
    /// (daylight saving time) and `tm_nsec` (nanoseconds of the date from 1970)
    /// are set to 0 since they aren't available in the PDF time format. They could,
    /// however, be calculated manually
    impl TryFrom<super::DateTime> for OffsetDateTime {
        type Error = time::Error;

        fn try_from(value: super::DateTime) -> Result<OffsetDateTime, Self::Error> {
            use time::Date;
            use time::format_description::parse_borrowed;

            // A `Z` means the time is UT (PDF 32000-1 §7.9.4).
            let text = match value.0.strip_suffix('Z') {
                Some(text) => format!("{text}+0000"),
                None => value.0,
            };
            let parse = |format: &str| -> Result<OffsetDateTime, Self::Error> {
                Ok(OffsetDateTime::parse(&text, &parse_borrowed::<3>(format)?)?)
            };

            // Like the chrono and jiff conversions, also accept a date without seconds or
            // without a time of day.
            parse("[year][month][day][hour][minute][second][offset_hour sign:mandatory][offset_minute]")
                .or_else(|_| parse("[year][month][day][hour][minute][offset_hour sign:mandatory][offset_minute]"))
                .or_else(|_| {
                    let date = Date::parse(&text, &parse_borrowed::<3>("[year][month][day]")?)?;
                    Ok(date.midnight().assume_utc())
                })
        }
    }
}

/// A PDF date carrying a fixed UTC offset: `D:YYYYMMDDHHMMSS+HH'mm'`
/// (ISO 32000-1, 7.9.4). The offset uses an apostrophe where the common
/// `%z`/`%:z` conventions use a colon, so the raw formatted bytes need
/// fixing up.
#[cfg(any(feature = "chrono", feature = "jiff"))]
fn date_with_offset(text: String) -> Object {
    let mut bytes = text.into_bytes();
    convert_utc_offset(&mut bytes);
    Object::string_literal(bytes)
}

/// Wrap date text that is already in PDF form. Use this for a date at UTC
/// (`D:YYYYMMDDHHMMSSZ`) and for the `time` crate's offset form, which spells
/// the `'` separator itself; only [`date_with_offset`] needs the fixup.
#[cfg(any(feature = "chrono", feature = "jiff", feature = "time"))]
fn date_literal(text: String) -> Object {
    Object::string_literal(text)
}

// Find the last `:` and turn it into an `'` to account for PDF weirdness
#[allow(dead_code)]
fn convert_utc_offset(bytes: &mut [u8]) {
    let mut index = bytes.len();
    while let Some(last) = bytes[..index].last_mut() {
        if *last == b':' {
            *last = b'\'';
            break;
        }
        index -= 1;
    }
}

#[derive(Clone, Debug)]
pub struct DateTime(String);

impl Object {
    // Parses the `D`, `:` and `\` out of a `Object::String` to parse the date time
    fn datetime_string(&self) -> Option<String> {
        if let Object::String(bytes, _) = self {
            String::from_utf8(bytes.iter().filter(|b| !b"D:'".contains(b)).cloned().collect()).ok()
        } else {
            None
        }
    }

    pub fn as_datetime(&self) -> Option<DateTime> {
        self.datetime_string().map(DateTime)
    }
}

impl DateTime {
    /// The date's characters with `D`, `:` and `'` removed.
    ///
    /// Every typed conversion of this value needs a date backend — `chrono`,
    /// `jiff` or `time` — but [`Object::as_datetime`] itself needs none. A
    /// build that enables no backend could therefore obtain a `DateTime` and
    /// have no way to read it. This is that way.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
