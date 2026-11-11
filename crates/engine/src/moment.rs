//! Independent Rust formatter for Moment's built-in date-token contract.
//! Locale tables are derived from MIT-licensed Moment 2.30.1; see locale_data.
//! This is formatting, not Moment parsing/arithmetic or dynamic JavaScript locale
//! callbacks. Differential evidence scopes are recorded in tests/fixtures.
use crate::daily::DailyError;
use regress::Regex;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::LazyLock};
use time::{Date, OffsetDateTime};

const TOKENS: &str = r"(\[[^\[]*\])|(\\)?([Hh]mm(ss)?|Mo|MM?M?M?|Do|DDDo|DD?D?D?|ddd?d?|do?|w[o|w]?|W[o|W]?|Qo?|N{1,5}|YYYYYY|YYYYY|YYYY|YY|y{2,4}|yo?|gg(ggg?)?|GG(GGG?)?|e|E|a|A|hh?|HH?|kk?|mm?|ss?|S{1,9}|x|X|zz?|ZZ?|.)";
const PRESETS: &str = r"(\[[^\[]*\])|(\\)?(LTS|LT|LL?L?L?|l{1,4})";
const MONTH_CONTEXT: &str = r"D[oD]?(\[[^\[\]]*\]|\s)+MMMM?";
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(TOKENS).unwrap());
static PRESET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(PRESETS).unwrap());
static DEFAULT_MONTH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(MONTH_CONTEXT).unwrap());
static LOCALES: LazyLock<BTreeMap<String, Locale>> = LazyLock::new(|| {
    let raw: Vec<RawLocale> = serde_json::from_str(include_str!("locale_data/moment-2.30.1.json"))
        .expect("validated locale data");
    raw.into_iter()
        .map(|data| {
            let locale = Locale {
                month_rule: data.month_rule.as_ref().map(|s| Regex::new(s).unwrap()),
                month_short_rule: data
                    .month_short_rule
                    .as_ref()
                    .map(|s| Regex::new(s).unwrap()),
                weekday_rule: data.weekday_rule.as_ref().map(|s| Regex::new(s).unwrap()),
                data,
            };
            (locale.data.id.clone(), locale)
        })
        .collect()
});
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLocale {
    id: String,
    months: Vec<Names>,
    months_short: Vec<Names>,
    weekdays: Vec<Names>,
    weekdays_short: Vec<String>,
    weekdays_min: Vec<String>,
    week: Week,
    presets: BTreeMap<String, String>,
    ordinal_profiles: Vec<OrdinalProfile>,
    ordinals: BTreeMap<String, usize>,
    meridiem: Vec<Meridiem>,
    postformat: BTreeMap<String, String>,
    eras: Vec<Era>,
    month_rule: Option<String>,
    month_short_rule: Option<String>,
    weekday_rule: Option<String>,
}
#[derive(Deserialize)]
struct Names {
    format: String,
    names: Vec<String>,
}
#[derive(Deserialize)]
struct Week {
    dow: i32,
    doy: i32,
}
#[derive(Deserialize)]
struct OrdinalProfile {
    cycle: Vec<OrdinalValue>,
    exceptions: BTreeMap<String, OrdinalValue>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum OrdinalValue {
    Affix([String; 2]),
    Literal(String),
}
#[derive(Deserialize)]
struct Meridiem {
    start: u16,
    labels: [String; 2],
}
#[derive(Deserialize)]
struct Era {
    since: String,
    until: String,
    offset: i32,
    name: String,
    abbr: String,
    narrow: String,
}
struct Locale {
    data: RawLocale,
    month_rule: Option<Regex>,
    month_short_rule: Option<Regex>,
    weekday_rule: Option<Regex>,
}
impl Locale {
    fn ordinal(&self, n: i32, period: &str) -> String {
        let profile = &self.data.ordinal_profiles[self.data.ordinals[period]];
        let value = profile
            .exceptions
            .get(&n.to_string())
            .unwrap_or(&profile.cycle[n.rem_euclid(profile.cycle.len() as i32) as usize]);
        match value {
            OrdinalValue::Affix([prefix, suffix]) => format!("{prefix}{n}{suffix}"),
            OrdinalValue::Literal(text) => text.clone(),
        }
    }
    fn month(&self, index: usize, short: bool, format: &str) -> &str {
        let list = if short {
            &self.data.months_short
        } else {
            &self.data.months
        };
        let rule = if short {
            self.month_short_rule.as_ref()
        } else {
            self.month_rule.as_ref()
        };
        let variant = if short
            && matches!(
                self.data.id.as_str(),
                "es" | "es-do" | "es-mx" | "es-us" | "fy" | "nl" | "nl-be"
            )
            && format.contains("-MMM-")
        {
            2
        } else if self.data.id == "el" && !short {
            usize::from(
                format
                    .find("MMMM")
                    .is_some_and(|at| format[..at].contains('D')),
            )
        } else if self.data.id == "pl" && !short {
            usize::from(format.contains("D MMMM"))
        } else if let Some(rule) = rule {
            if rule.find(format).is_some() {
                list.iter()
                    .position(|names| rule.find(&names.format).is_some())
                    .unwrap_or(1)
            } else {
                0
            }
        } else {
            usize::from(DEFAULT_MONTH_RE.find(format).is_some())
        };
        &list[variant].names[index]
    }
    fn weekday(&self, index: usize, format: &str) -> &str {
        let id = self.data.id.as_str();
        let variant = if id == "uk" {
            static ACC: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"(\[[ВвУу]\]) ?dddd").unwrap());
            static GEN: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"\[?(?:минулої|наступної)? ?\] ?dddd").unwrap());
            if ACC.find(format).is_some() {
                5
            } else if GEN.find(format).is_some() {
                6
            } else {
                0
            }
        } else if self
            .weekday_rule
            .as_ref()
            .is_some_and(|rule| rule.find(format).is_some())
        {
            self.data
                .weekdays
                .iter()
                .position(|names| {
                    self.weekday_rule
                        .as_ref()
                        .unwrap()
                        .find(&names.format)
                        .is_some()
                })
                .unwrap_or(0)
        } else {
            0
        };
        &self.data.weekdays[variant].names[index]
    }
    fn meridiem(&self, hour: u8, minute: u8, lower: bool) -> &str {
        let position = u16::from(hour) * 60 + u16::from(minute);
        let index = self
            .data
            .meridiem
            .partition_point(|entry| entry.start <= position)
            - 1;
        &self.data.meridiem[index].labels[usize::from(lower)]
    }
    fn postformat(&self, text: &str) -> String {
        text.chars()
            .map(|ch| {
                self.data
                    .postformat
                    .get(&ch.to_string())
                    .cloned()
                    .unwrap_or_else(|| ch.to_string())
            })
            .collect()
    }
    fn era(&self, date: Date) -> Option<(&Era, i32)> {
        let day = i64::from(date.to_julian_day());
        for era in &self.data.eras {
            let since = era_day(&era.since)?;
            let until = era_day(&era.until)?;
            if (since <= day && day <= until) || (until <= day && day <= since) {
                let year = OffsetDateTime::from_unix_timestamp_nanos(
                    era.since.parse::<i128>().ok()? * 1_000_000,
                )
                .ok()?
                .year();
                return Some((
                    era,
                    (date.year() - year) * if since <= until { 1 } else { -1 } + era.offset,
                ));
            }
        }
        None
    }
}
fn era_day(value: &str) -> Option<i64> {
    match value {
        "Infinity" => Some(i64::MAX),
        "-Infinity" => Some(i64::MIN),
        _ => OffsetDateTime::from_unix_timestamp_nanos(value.parse::<i128>().ok()? * 1_000_000)
            .ok()
            .map(|time| i64::from(time.date().to_julian_day())),
    }
}
/// Locale lookup tries progressively shorter locale identifiers; an unknown
/// identifier returns None so an Engine can retain its previous active locale.
pub fn resolve_locale(id: &str) -> Option<&'static str> {
    let normalized = id.to_ascii_lowercase().replacen('_', "-", 1);
    let mut candidate = normalized.as_str();
    loop {
        if let Some((id, _)) = LOCALES.get_key_value(candidate) {
            return Some(id);
        }
        if let Some((parent, _)) = candidate.rsplit_once('-') {
            candidate = parent;
        } else {
            return None;
        }
    }
}
pub fn locale_ids() -> impl Iterator<Item = &'static str> {
    LOCALES.keys().map(String::as_str)
}

pub fn format(now: OffsetDateTime, pattern: &str, locale: &str) -> Result<String, DailyError> {
    let id = resolve_locale(locale).unwrap_or("en");
    let locale = &LOCALES[id];
    let mut expanded = pattern.to_owned();
    for _ in 0..6 {
        let mut next = String::new();
        let mut end = 0;
        let mut changed = false;
        for found in PRESET_RE.find_iter(&expanded) {
            next.push_str(&expanded[end..found.range.start]);
            let text = &expanded[found.range.clone()];
            let replacement = if found.group(3).is_some() && found.group(2).is_none() {
                locale.data.presets.get(text)
            } else {
                None
            };
            if let Some(replacement) = replacement {
                next.push_str(replacement);
                changed = true;
            } else {
                next.push_str(text);
            }
            end = found.range.end;
        }
        next.push_str(&expanded[end..]);
        expanded = next;
        if !changed {
            break;
        }
    }
    let mut output = String::new();
    for found in TOKEN_RE.find_iter(&expanded) {
        let token = &expanded[found.range.clone()];
        if token.starts_with('[') && token.ends_with(']') && token.len() > 1 {
            output.push_str(&token[1..token.len() - 1]);
            continue;
        }
        if token.starts_with('\\') {
            output.push_str(&token.replace('\\', ""));
            continue;
        }
        output.push_str(&token_value(now, token, &expanded, locale));
    }
    Ok(locale.postformat(&output))
}
fn pad(number: i32, width: usize, force_sign: bool) -> String {
    let sign = if number < 0 {
        "-"
    } else if force_sign {
        "+"
    } else {
        ""
    };
    format!("{sign}{:0width$}", number.unsigned_abs())
}
fn first_week_offset(year: i32, dow: i32, doy: i32) -> i32 {
    let fwd = 7 + dow - doy;
    // Proleptic Gregorian weekday arithmetic also supports the adjacent year
    // outside time::Date's representable range at +/-9999 boundaries.
    let prior = i64::from(year) - 1;
    let days = 365 * prior + prior.div_euclid(4) - prior.div_euclid(100) + prior.div_euclid(400);
    // Moment createUTCDate constructs years 0..99 via year+400, then resets
    // fullYear. A negative January day rolls into December before that reset,
    // retaining the requested year instead of the preceding year (ar-kw doy12).
    let rollover = if (0..100).contains(&year) && fwd <= 0 {
        if time::util::is_leap_year(year) {
            366
        } else {
            365
        }
    } else {
        0
    };
    let weekday = (days + i64::from(fwd) + rollover).rem_euclid(7) as i32;
    let local = (7 + weekday - dow).rem_euclid(7);
    -local + fwd - 1
}
fn weeks_in_year(year: i32, dow: i32, doy: i32) -> f64 {
    let days = if time::util::is_leap_year(year) {
        366
    } else {
        365
    };
    f64::from(days - first_week_offset(year, dow, doy) + first_week_offset(year + 1, dow, doy))
        / 7.0
}
fn week(date: Date, dow: i32, doy: i32) -> (i32, f64) {
    let year = date.year();
    let number = f64::from(
        (i32::from(date.ordinal()) - first_week_offset(year, dow, doy) - 1).div_euclid(7) + 1,
    );
    if number < 1.0 {
        (year - 1, number + weeks_in_year(year - 1, dow, doy))
    } else if number > weeks_in_year(year, dow, doy) {
        (year + 1, number - weeks_in_year(year, dow, doy))
    } else {
        (year, number)
    }
}
fn pad_week(n: f64) -> String {
    let text = n.to_string();
    if text.len() < 2 {
        format!("0{text}")
    } else {
        text
    }
}
fn ordinal_week(locale: &Locale, n: f64, token: &str) -> String {
    if n.fract() == 0.0 {
        locale.ordinal(n as i32, token)
    } else {
        // Only ar-kw's built-in rule yields a fraction; its ordinal is identity.
        n.to_string()
    }
}
fn token_value(now: OffsetDateTime, token: &str, pattern: &str, locale: &Locale) -> String {
    let (iso_year, iso_week) = week(now.date(), 1, 4);
    let (week_year, local_week) = week(now.date(), locale.data.week.dow, locale.data.week.doy);
    let hour = if now.hour() % 12 == 0 {
        12
    } else {
        now.hour() % 12
    };
    let day = i32::from(now.weekday().number_days_from_sunday());
    match token {
        "Y" => {
            if now.year() <= 9999 {
                pad(now.year(), 4, false)
            } else {
                format!("+{}", now.year())
            }
        }
        "YY" => pad(now.year() % 100, 2, false),
        "YYYY" => pad(now.year(), 4, false),
        "YYYYY" => pad(now.year(), 5, false),
        "YYYYYY" => pad(now.year(), 6, true),
        "M" => (now.month() as u8).to_string(),
        "MM" => pad(now.month() as i32, 2, false),
        "MMM" => locale.month(now.month() as usize - 1, true, pattern).into(),
        "MMMM" => locale
            .month(now.month() as usize - 1, false, pattern)
            .into(),
        "Mo" => locale.ordinal(now.month() as i32, "M"),
        "Q" => ((now.month() as i32 - 1) / 3 + 1).to_string(),
        "Qo" => locale.ordinal((now.month() as i32 - 1) / 3 + 1, "Q"),
        "D" => now.day().to_string(),
        "DD" => pad(now.day() as i32, 2, false),
        "Do" => locale.ordinal(now.day() as i32, "D"),
        "DDD" => now.ordinal().to_string(),
        "DDDD" => pad(now.ordinal() as i32, 3, false),
        "DDDo" => locale.ordinal(now.ordinal() as i32, "DDD"),
        "d" => day.to_string(),
        "do" => locale.ordinal(day, "d"),
        "dd" => locale.data.weekdays_min[day as usize].clone(),
        "ddd" => locale.data.weekdays_short[day as usize].clone(),
        "dddd" => locale.weekday(day as usize, pattern).into(),
        "e" => ((day + 7 - locale.data.week.dow) % 7).to_string(),
        "E" => now.weekday().number_from_monday().to_string(),
        "w" => local_week.to_string(),
        "ww" => pad_week(local_week),
        "wo" => ordinal_week(locale, local_week, "w"),
        "W" => iso_week.to_string(),
        "WW" => pad_week(iso_week),
        "Wo" => ordinal_week(locale, iso_week, "W"),
        "gg" => pad(week_year % 100, 2, false),
        "gggg" => pad(week_year, 4, false),
        "ggggg" => pad(week_year, 5, false),
        "GG" => pad(iso_year % 100, 2, false),
        "GGGG" => pad(iso_year, 4, false),
        "GGGGG" => pad(iso_year, 5, false),
        "H" => now.hour().to_string(),
        "HH" => pad(now.hour() as i32, 2, false),
        "h" => hour.to_string(),
        "hh" => pad(hour as i32, 2, false),
        "k" => if now.hour() == 0 { 24 } else { now.hour() }.to_string(),
        "kk" => pad(
            if now.hour() == 0 { 24 } else { now.hour() } as i32,
            2,
            false,
        ),
        "Hmm" => format!("{}{:02}", now.hour(), now.minute()),
        "Hmmss" => format!("{}{:02}{:02}", now.hour(), now.minute(), now.second()),
        "hmm" => format!("{hour}{:02}", now.minute()),
        "hmmss" => format!("{hour}{:02}{:02}", now.minute(), now.second()),
        "m" => now.minute().to_string(),
        "mm" => pad(now.minute() as i32, 2, false),
        "s" => now.second().to_string(),
        "ss" => pad(now.second() as i32, 2, false),
        "a" => locale.meridiem(now.hour(), now.minute(), true).into(),
        "A" => locale.meridiem(now.hour(), now.minute(), false).into(),
        "S" => (now.millisecond() / 100).to_string(),
        "SS" => pad(now.millisecond() as i32 / 10, 2, false),
        token if token.bytes().all(|ch| ch == b'S') && token.len() <= 9 => {
            format!("{:03}{}", now.millisecond(), "0".repeat(token.len() - 3))
        }
        "X" => now.unix_timestamp().to_string(),
        "x" => now.unix_timestamp_nanos().div_euclid(1_000_000).to_string(),
        "Z" | "ZZ" => {
            let seconds = now.offset().whole_seconds();
            let sign = if seconds < 0 { '-' } else { '+' };
            let minutes = seconds.unsigned_abs() / 60;
            if token == "Z" {
                format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
            } else {
                format!("{sign}{:02}{:02}", minutes / 60, minutes % 60)
            }
        }
        "z" | "zz" => String::new(), // daily/template moments are local, including TZ=UTC.
        "N" | "NN" | "NNN" => locale
            .era(now.date())
            .map(|(era, _)| era.abbr.clone())
            .unwrap_or_default(),
        "NNNN" => locale
            .era(now.date())
            .map(|(era, _)| era.name.clone())
            .unwrap_or_default(),
        "NNNNN" => locale
            .era(now.date())
            .map(|(era, _)| era.narrow.clone())
            .unwrap_or_default(),
        "y" | "yy" | "yyy" | "yyyy" => pad(
            locale
                .era(now.date())
                .map(|(_, year)| year)
                .unwrap_or(now.year()),
            token.len(),
            false,
        ),
        "yo" => locale.ordinal(
            locale
                .era(now.date())
                .map(|(_, year)| year)
                .unwrap_or(now.year()),
            "y",
        ),
        _ => token.into(),
    }
}
