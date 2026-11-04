//! Text entry for the date picker: ISO 8601-style dates and times, optional
//! UTC offsets, and the relative words `now`, `today`, `yesterday` and
//! `tomorrow`.
//!
//! Relative words resolve against a caller-supplied clock (the picker never
//! reads the system clock), so hosts decide which time zone "now" belongs to.

use chrono::{Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

use super::{DatePickerMode, add_days};

/// What the parser needs besides the text.
#[derive(Debug, Clone, Copy)]
pub(super) struct ParseContext {
    pub mode: DatePickerMode,
    /// The current wall-clock time in the host's zone, if known.
    pub now: Option<NaiveDateTime>,
    /// Today's date, for date words when only the date is known.
    pub today: Option<NaiveDate>,
    /// The offset `now` is in, used when `now` is typed into a field that
    /// takes offsets.
    pub now_offset: Option<FixedOffset>,
    /// Whether a trailing UTC offset is accepted.
    pub offsets: bool,
}

/// How a parsed text changes the offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OffsetChange {
    /// Leave the offset as it is (relative date words).
    Keep,
    /// Use this offset (`None`: none was typed).
    Set(Option<FixedOffset>),
}

/// The parts of the value a text sets; parts left `None` keep their value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Parsed {
    pub date: Option<NaiveDate>,
    pub time: Option<NaiveTime>,
    pub offset: OffsetChange,
}

/// Why a text is not a value yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ParseError {
    /// The text could still become valid by typing more.
    Incomplete,
    /// The text cannot become valid; the message says why.
    Invalid(String),
}

/// Parse `text` for a picker in `context.mode`.
pub(super) fn parse(text: &str, context: &ParseContext) -> Result<Parsed, ParseError> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Err(ParseError::Incomplete);
    }
    if let Some(parsed) = relative(&text, context)? {
        return Ok(parsed);
    }
    let mut cursor = Cursor::new(&text);
    let (date, time, offset) = match context.mode {
        DatePickerMode::Date => {
            let date = cursor.date()?;
            cursor.end("this field takes a date only")?;
            (Some(date), None, OffsetChange::Keep)
        }
        DatePickerMode::Time => {
            if looks_like_date(&text) {
                return Err(ParseError::Invalid("this field takes a time only".into()));
            }
            let time = cursor.time()?;
            let offset = cursor.offset(context.offsets)?;
            cursor.end("unexpected text after the time")?;
            (None, Some(time), OffsetChange::Set(offset))
        }
        DatePickerMode::DateTime => {
            if looks_like_date(&text) {
                let date = cursor.date()?;
                if cursor.at_end() {
                    (Some(date), None, OffsetChange::Keep)
                } else {
                    cursor.date_time_separator()?;
                    let time = cursor.time()?;
                    let offset = cursor.offset(context.offsets)?;
                    cursor.end("unexpected text after the time")?;
                    (Some(date), Some(time), OffsetChange::Set(offset))
                }
            } else {
                let time = cursor.time()?;
                let offset = cursor.offset(context.offsets)?;
                cursor.end("unexpected text after the time")?;
                (None, Some(time), OffsetChange::Set(offset))
            }
        }
    };
    Ok(Parsed { date, time, offset })
}

/// Words the picker accepts, for hints and completion of partial input.
const WORDS: [&str; 4] = ["now", "today", "yesterday", "tomorrow"];

/// `now`, or a date word optionally followed by a time.
fn relative(text: &str, context: &ParseContext) -> Result<Option<Parsed>, ParseError> {
    let (word, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(word, rest)| (word, rest.trim()));
    if !word.starts_with(|ch: char| ch.is_ascii_alphabetic()) {
        return Ok(None);
    }
    if !WORDS.contains(&word) {
        if rest.is_empty() && WORDS.iter().any(|known| known.starts_with(word)) {
            return Err(ParseError::Incomplete);
        }
        return Err(ParseError::Invalid(format!(
            "unknown word \u{201c}{word}\u{201d}: try now, today, yesterday or tomorrow"
        )));
    }
    let unavailable = || {
        ParseError::Invalid(format!(
            "\u{201c}{word}\u{201d} needs the current time, which is unavailable"
        ))
    };
    if word == "now" {
        let now = context.now.ok_or_else(unavailable)?;
        if !rest.is_empty() {
            return Err(ParseError::Invalid(
                "unexpected text after \u{201c}now\u{201d}".into(),
            ));
        }
        return Ok(Some(match context.mode {
            DatePickerMode::Date => Parsed {
                date: Some(now.date()),
                time: None,
                offset: OffsetChange::Keep,
            },
            DatePickerMode::DateTime | DatePickerMode::Time => Parsed {
                date: (context.mode == DatePickerMode::DateTime).then_some(now.date()),
                time: Some(now.time()),
                offset: OffsetChange::Set(context.now_offset.filter(|_| context.offsets)),
            },
        }));
    }
    if context.mode == DatePickerMode::Time {
        return Err(ParseError::Invalid("this field takes a time only".into()));
    }
    let days = match word {
        "yesterday" => -1,
        "tomorrow" => 1,
        _ => 0,
    };
    let today = context
        .today
        .or_else(|| context.now.map(|now| now.date()))
        .ok_or_else(unavailable)?;
    let date = add_days(today, days)
        .ok_or_else(|| ParseError::Invalid("that date is out of range".into()))?;
    if rest.is_empty() {
        return Ok(Some(Parsed {
            date: Some(date),
            time: None,
            offset: OffsetChange::Keep,
        }));
    }
    if context.mode == DatePickerMode::Date {
        return Err(ParseError::Invalid("this field takes a date only".into()));
    }
    let mut cursor = Cursor::new(rest);
    let time = cursor.time()?;
    let offset = cursor.offset(context.offsets)?;
    cursor.end("unexpected text after the time")?;
    Ok(Some(Parsed {
        date: Some(date),
        time: Some(time),
        offset: OffsetChange::Set(offset),
    }))
}

/// Whether `text` starts with a year (four digits, or digits then `-` / `/`).
fn looks_like_date(text: &str) -> bool {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    digits >= 3 || matches!(text.as_bytes().get(digits), Some(b'-' | b'/'))
}

/// Canonical text for a value.
pub(super) fn format(
    value: NaiveDateTime,
    offset: Option<FixedOffset>,
    mode: DatePickerMode,
    seconds: bool,
) -> String {
    let mut text = match mode {
        DatePickerMode::Date => return date_text(value.date()),
        DatePickerMode::DateTime => {
            format!(
                "{} {}",
                date_text(value.date()),
                time_text(value.time(), seconds)
            )
        }
        DatePickerMode::Time => time_text(value.time(), seconds),
    };
    if let Some(offset) = offset {
        text.push_str(&offset.to_string());
    }
    text
}

/// `2026-11-04`.
fn date_text(date: NaiveDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// `14:30:05` (with any fraction: `14:30:05.25`), or `14:30` without
/// seconds.
fn time_text(time: NaiveTime, seconds: bool) -> String {
    if !seconds {
        return format!("{:02}:{:02}", time.hour(), time.minute());
    }
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        time.hour(),
        time.minute(),
        time.second()
    );
    let nanos = time.nanosecond() % 1_000_000_000;
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        text.push('.');
        text.push_str(fraction.trim_end_matches('0'));
    }
    text
}

/// A left-to-right reader over the lowercased text.
struct Cursor<'a> {
    text: &'a str,
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(text: &'a str) -> Self {
        Self { text, position: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.position..]
    }

    fn at_end(&self) -> bool {
        self.rest().is_empty()
    }

    fn peek(&self) -> Option<u8> {
        self.rest().bytes().next()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn skip_spaces(&mut self) {
        while self.peek() == Some(b' ') {
            self.position += 1;
        }
    }

    /// At most `max` digits, `None` when there are none.
    fn digits(&mut self, max: usize) -> Option<(u32, usize)> {
        let count = self
            .rest()
            .bytes()
            .take(max)
            .take_while(u8::is_ascii_digit)
            .count();
        if count == 0 {
            return None;
        }
        let value = self.rest()[..count].parse().ok()?;
        self.position += count;
        Some((value, count))
    }

    /// `what` was required next: incomplete at the end of the text, invalid
    /// otherwise.
    fn expected(&self, what: &str) -> ParseError {
        if self.at_end() {
            ParseError::Incomplete
        } else {
            ParseError::Invalid(format!("expected {what}"))
        }
    }

    fn date(&mut self) -> Result<NaiveDate, ParseError> {
        let (year, year_digits) = self.digits(4).ok_or_else(|| self.expected("a year"))?;
        if year_digits < 4 {
            return Err(self.expected("a four-digit year"));
        }
        let Some(separator @ (b'-' | b'/')) = self.peek() else {
            return Err(self.expected("\u{2013} after the year"));
        };
        self.position += 1;
        let (month, month_digits) = self.digits(2).ok_or_else(|| self.expected("a month"))?;
        // `0` may still become `01`-`09`; anything else is checked now.
        if (month_digits == 2 || month > 1) && !(1..=12).contains(&month) {
            return Err(ParseError::Invalid(format!(
                "month {month} is out of range"
            )));
        }
        if !self.eat(separator) {
            return Err(self.expected("\u{2013} after the month"));
        }
        if month == 0 {
            return Err(ParseError::Invalid("month 0 is out of range".into()));
        }
        let (day, day_digits) = self.digits(2).ok_or_else(|| self.expected("a day"))?;
        let year = i32::try_from(year).unwrap_or(i32::MAX);
        NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| {
            // `0` may still become `01`-`09`.
            if day == 0 && day_digits < 2 && self.at_end() {
                ParseError::Incomplete
            } else if day == 0 {
                ParseError::Invalid("day 0 is out of range".into())
            } else {
                ParseError::Invalid(format!(
                    "{} {year} has {} days",
                    month_name(month),
                    days_in(year, month)
                ))
            }
        })
    }

    fn date_time_separator(&mut self) -> Result<(), ParseError> {
        if self.eat(b't') {
            return Ok(());
        }
        if self.peek() == Some(b' ') {
            self.skip_spaces();
            return Ok(());
        }
        Err(ParseError::Invalid(
            "expected a space or T before the time".into(),
        ))
    }

    fn time(&mut self) -> Result<NaiveTime, ParseError> {
        let (hour, _) = self.digits(2).ok_or_else(|| self.expected("an hour"))?;
        if hour > 23 {
            return Err(ParseError::Invalid(format!("hour {hour} is out of range")));
        }
        if !self.eat(b':') {
            return Err(self.expected(": after the hour"));
        }
        let minute = self.two_digits("minutes")?;
        let mut second = 0;
        let mut nanos = 0;
        if self.eat(b':') {
            second = self.two_digits("seconds")?;
            if self.eat(b'.') {
                let start = self.position;
                let (_, count) = self
                    .digits(9)
                    .ok_or_else(|| self.expected("fractional seconds"))?;
                let digits = &self.text[start..start + count];
                nanos = format!("{digits:0<9}").parse().unwrap_or(0);
                // Further precision is accepted and dropped.
                while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    self.position += 1;
                }
            }
        }
        NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)
            .ok_or_else(|| ParseError::Invalid("that time is out of range".into()))
    }

    fn two_digits(&mut self, what: &str) -> Result<u32, ParseError> {
        let (value, count) = self.digits(2).ok_or_else(|| self.expected(what))?;
        if count < 2 {
            return Err(if self.at_end() {
                ParseError::Incomplete
            } else {
                ParseError::Invalid(format!("{what} take two digits"))
            });
        }
        if value > 59 {
            return Err(ParseError::Invalid(format!(
                "{what} {value} are out of range"
            )));
        }
        Ok(value)
    }

    /// An optional trailing offset: `Z`, `UTC`, `±HH`, `±HHMM` or `±HH:MM`.
    fn offset(&mut self, allowed: bool) -> Result<Option<FixedOffset>, ParseError> {
        let start = self.position;
        self.skip_spaces();
        if self.at_end() {
            self.position = start;
            return Ok(None);
        }
        let rest = self.rest();
        let offset = if rest == "z" || rest == "utc" {
            self.position = self.text.len();
            FixedOffset::east_opt(0)
        } else if "utc".starts_with(rest) {
            return Err(ParseError::Incomplete);
        } else if let Some(sign) = self.peek().filter(|byte| matches!(byte, b'+' | b'-')) {
            self.position += 1;
            let (hours, hour_digits) = self
                .digits(2)
                .ok_or_else(|| self.expected("offset hours"))?;
            if hour_digits < 2 && self.at_end() {
                return Err(ParseError::Incomplete);
            }
            self.eat(b':');
            let minutes = if self.at_end() {
                0
            } else {
                self.two_digits("offset minutes")?
            };
            if hours > 15 {
                return Err(ParseError::Invalid(format!(
                    "offset {hours} hours is out of range"
                )));
            }
            let seconds = i32::try_from(hours * 3600 + minutes * 60).unwrap_or(i32::MAX);
            FixedOffset::east_opt(if sign == b'-' { -seconds } else { seconds })
        } else {
            self.position = start;
            return Ok(None);
        };
        if !allowed {
            return Err(ParseError::Invalid(
                "this field takes no time zone offset".into(),
            ));
        }
        Ok(offset)
    }

    fn end(&self, message: &str) -> Result<(), ParseError> {
        if self.at_end() {
            Ok(())
        } else {
            Err(ParseError::Invalid(message.into()))
        }
    }
}

fn days_in(year: i32, month: u32) -> u32 {
    (28..=31)
        .rev()
        .find(|day| NaiveDate::from_ymd_opt(year, month, *day).is_some())
        .unwrap_or(28)
}

fn month_name(month: u32) -> &'static str {
    super::ENGLISH_MONTH_NAMES
        .get(usize::try_from(month.saturating_sub(1)).unwrap_or(0))
        .copied()
        .unwrap_or("")
}

/// A short, readable rendering of a value: `We 4 Nov 2026, 14:30:05 +02:00`.
pub(super) fn describe(
    value: NaiveDateTime,
    offset: Option<FixedOffset>,
    mode: DatePickerMode,
    seconds: bool,
    month_names: &[&str; 12],
    weekday_names: &[&str; 7],
) -> String {
    let weekday =
        weekday_names[usize::try_from(value.weekday().num_days_from_monday()).unwrap_or(0)];
    let month: String = month_names[usize::try_from(value.month0()).unwrap_or(0)]
        .chars()
        .take(3)
        .collect();
    let date = format!("{weekday} {} {month} {}", value.day(), value.year());
    let time = time_text(value.time(), seconds);
    let offset = offset
        .map(|offset| format!(" {offset}"))
        .unwrap_or_default();
    match mode {
        DatePickerMode::Date => date,
        DatePickerMode::DateTime => format!("{date}, {time}{offset}"),
        DatePickerMode::Time => format!("{time}{offset}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(mode: DatePickerMode) -> ParseContext {
        ParseContext {
            mode,
            now: NaiveDate::from_ymd_opt(2026, 11, 4)
                .unwrap()
                .and_hms_opt(23, 59, 30),
            today: None,
            now_offset: FixedOffset::east_opt(-5 * 3600),
            offsets: true,
        }
    }

    fn ok(text: &str, mode: DatePickerMode) -> Parsed {
        parse(text, &context(mode)).unwrap_or_else(|error| panic!("{text:?}: {error:?}"))
    }

    fn date(year: i32, month: u32, day: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(year, month, day)
    }

    fn time(h: u32, m: u32, s: u32) -> Option<NaiveTime> {
        NaiveTime::from_hms_opt(h, m, s)
    }

    #[test]
    fn iso_dates_and_times() {
        let parsed = ok("2026-11-04 14:30:05", DatePickerMode::DateTime);
        assert_eq!(
            (parsed.date, parsed.time),
            (date(2026, 11, 4), time(14, 30, 5))
        );
        assert_eq!(parsed.offset, OffsetChange::Set(None));
        let parsed = ok("2026/1/9T7:05", DatePickerMode::DateTime);
        assert_eq!(
            (parsed.date, parsed.time),
            (date(2026, 1, 9), time(7, 5, 0))
        );
        // A date alone keeps the time; a time alone keeps the date.
        let parsed = ok("2026-02-28", DatePickerMode::DateTime);
        assert_eq!((parsed.date, parsed.time), (date(2026, 2, 28), None));
        let parsed = ok("14:30", DatePickerMode::DateTime);
        assert_eq!((parsed.date, parsed.time), (None, time(14, 30, 0)));
        let parsed = ok("10:00:00.250", DatePickerMode::Time);
        assert_eq!(parsed.time, NaiveTime::from_hms_milli_opt(10, 0, 0, 250));
    }

    #[test]
    fn offsets_are_parsed_only_where_allowed() {
        for (text, hours) in [
            ("2026-11-04 14:30+02:00", 2),
            ("2026-11-04 14:30 -0530", -5),
            ("2026-11-04 14:30Z", 0),
            ("2026-11-04 14:30 UTC", 0),
            ("2026-11-04 14:30+02", 2),
        ] {
            let OffsetChange::Set(Some(offset)) = ok(text, DatePickerMode::DateTime).offset else {
                panic!("{text}: no offset");
            };
            assert_eq!(offset.local_minus_utc() / 3600, hours, "{text}");
        }
        let strict = ParseContext {
            offsets: false,
            ..context(DatePickerMode::DateTime)
        };
        assert!(matches!(
            parse("2026-11-04 14:30+02:00", &strict),
            Err(ParseError::Invalid(message)) if message.contains("offset")
        ));
    }

    #[test]
    fn relative_words_use_the_supplied_clock_and_zone() {
        let parsed = ok("now", DatePickerMode::DateTime);
        assert_eq!(
            (parsed.date, parsed.time),
            (date(2026, 11, 4), time(23, 59, 30))
        );
        assert_eq!(
            parsed.offset,
            OffsetChange::Set(FixedOffset::east_opt(-5 * 3600))
        );
        // Date words keep the time and offset unless a time follows.
        let parsed = ok("Tomorrow", DatePickerMode::DateTime);
        assert_eq!((parsed.date, parsed.time), (date(2026, 11, 5), None));
        assert_eq!(parsed.offset, OffsetChange::Keep);
        let parsed = ok("yesterday 9:15", DatePickerMode::DateTime);
        assert_eq!(
            (parsed.date, parsed.time),
            (date(2026, 11, 3), time(9, 15, 0))
        );
        assert_eq!(ok("today", DatePickerMode::Date).date, date(2026, 11, 4));
        assert_eq!(ok("now", DatePickerMode::Time).time, time(23, 59, 30));
        let no_clock = ParseContext {
            now: None,
            ..context(DatePickerMode::Date)
        };
        assert!(matches!(
            parse("today", &no_clock),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn partial_input_is_incomplete_not_wrong() {
        for text in [
            "2",
            "202",
            "2026",
            "2026-",
            "2026-1",
            "2026-11-",
            "2026-11-0",
            "2026-11-04 1",
            "2026-11-04 14:",
            "2026-11-04 14:3",
            "tom",
            "n",
            "2026-11-04 14:30+0",
        ] {
            assert_eq!(
                parse(text, &context(DatePickerMode::DateTime)),
                Err(ParseError::Incomplete),
                "{text}"
            );
        }
    }

    #[test]
    fn mistakes_say_what_is_wrong() {
        for (text, mode, message) in [
            ("2026-13-01", DatePickerMode::Date, "month 13"),
            (
                "2026-02-30",
                DatePickerMode::Date,
                "February 2026 has 28 days",
            ),
            ("2026-11-04 25:00", DatePickerMode::DateTime, "hour 25"),
            ("2026-11-04 10:75", DatePickerMode::DateTime, "minutes 75"),
            ("2026-11-04 10:00", DatePickerMode::Date, "date only"),
            ("2026-11-04", DatePickerMode::Time, "time only"),
            ("someday", DatePickerMode::Date, "unknown word"),
            (
                "2026-11-04 10:00 later",
                DatePickerMode::DateTime,
                "unexpected text",
            ),
        ] {
            match parse(text, &context(mode)) {
                Err(ParseError::Invalid(error)) => {
                    assert!(error.contains(message), "{text}: {error}");
                }
                other => panic!("{text}: {other:?}"),
            }
        }
    }

    #[test]
    fn format_and_describe_round_trip() {
        let value = NaiveDate::from_ymd_opt(2026, 11, 4)
            .unwrap()
            .and_hms_opt(14, 30, 5)
            .unwrap();
        let offset = FixedOffset::east_opt(2 * 3600);
        let text = format(value, offset, DatePickerMode::DateTime, true);
        assert_eq!(text, "2026-11-04 14:30:05+02:00");
        let parsed = ok(&text, DatePickerMode::DateTime);
        assert_eq!(parsed.offset, OffsetChange::Set(offset));
        assert_eq!(format(value, None, DatePickerMode::Time, false), "14:30");
        let precise = value.with_nanosecond(123_450_000).unwrap();
        assert_eq!(
            format(precise, None, DatePickerMode::Time, true),
            "14:30:05.12345"
        );
        assert_eq!(
            describe(
                value,
                offset,
                DatePickerMode::DateTime,
                true,
                &super::super::ENGLISH_MONTH_NAMES,
                &super::super::ENGLISH_WEEKDAY_NAMES
            ),
            "We 4 Nov 2026, 14:30:05 +02:00"
        );
    }
}
