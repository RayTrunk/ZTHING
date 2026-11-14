use std::ops::{Range, RangeInclusive};

use memchr::memmem::{self, Finder};
use regex::bytes::{Regex, RegexBuilder};

use crate::index::fold_case_into;

const DAY: i64 = 86_400;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("invalid regex {pattern:?}: {source}")]
    Regex {
        pattern: String,
        source: regex::Error,
    },
    #[error("invalid size {0:?}, expected for example size:>10mb")]
    Size(String),
    #[error("invalid date {0:?}, expected today, yesterday, YYYY-MM-DD or a range a..b")]
    Date(String),
}

/// Reference point for relative dates such as `dm:today`.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    /// Unix seconds (UTC).
    pub unix: i64,
    /// Local offset from UTC in seconds, 3600 for CET.
    pub utc_offset: i64,
}

/// A parsed search. All space-separated groups must match. Within a group the
/// `|`-separated terms are alternatives, so `|` binds tighter than a space.
#[derive(Debug)]
pub struct Query {
    pub(crate) groups: Vec<Vec<Term>>,
}

#[derive(Debug)]
pub(crate) struct Term {
    /// Source text, used to recognize unchanged terms between two keystrokes.
    raw: String,
    pub(crate) negate: bool,
    pub(crate) case: bool,
    pub(crate) path: bool,
    pub(crate) only: Option<Only>,
    pub(crate) kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Only {
    Files,
    Folders,
}

#[derive(Debug)]
pub(crate) enum Kind {
    /// A modifier without a value, such as a bare `file:`.
    All,
    /// The needle is case-folded unless the term has `case:`.
    Text(Box<Finder<'static>>),
    /// `regex:` and wildcard terms.
    Regex(Regex),
    /// Case-folded extensions without the dot.
    Ext(Vec<String>),
    Size(RangeInclusive<u64>),
    /// Unix seconds.
    Modified(Range<i64>),
}

impl Query {
    pub fn parse(input: &str, now: Now) -> Result<Self, QueryError> {
        let groups = split_groups(input)
            .into_iter()
            .map(|group| group.into_iter().map(|raw| parse_term(raw, now)).collect())
            .collect::<Result<_, _>>()?;
        Ok(Self { groups })
    }

    /// True if every entry matching `self` also matches `previous`, so that
    /// `self` may be evaluated on the results of `previous` only.
    pub fn narrows(&self, previous: &Query) -> bool {
        previous.groups.len() <= self.groups.len()
            && previous.groups.iter().zip(&self.groups).all(|(old, new)| {
                old.iter().map(|t| &t.raw).eq(new.iter().map(|t| &t.raw))
                    || match (old.as_slice(), new.as_slice()) {
                        ([o], [n]) => {
                            !o.negate
                                && !n.negate
                                && o.case == n.case
                                && o.path == n.path
                                && o.only == n.only
                                && match (&o.kind, &n.kind) {
                                    (Kind::All, _) => true,
                                    (Kind::Text(o), Kind::Text(n)) => {
                                        memmem::find(n.needle(), o.needle()).is_some()
                                    }
                                    _ => false,
                                }
                        }
                        _ => false,
                    }
            })
    }
}

/// Splits at spaces and `|` outside of quotes. Quotes stay in the returned words
/// so that a quoted `"case:"` is not mistaken for a modifier.
fn split_groups(input: &str) -> Vec<Vec<&str>> {
    let bytes = input.as_bytes();
    let mut groups: Vec<Vec<&str>> = Vec::new();
    let mut or_pending = false;
    let mut quoted = false;
    let mut start = None;
    for i in 0..=bytes.len() {
        let delimiter = match bytes.get(i) {
            None => true,
            Some(b'"') => {
                quoted = !quoted;
                false
            }
            Some(&c) => !quoted && (c == b'|' || c.is_ascii_whitespace()),
        };
        if !delimiter {
            start.get_or_insert(i);
            continue;
        }
        if let Some(s) = start.take() {
            let word = &input[s..i];
            match groups.last_mut() {
                Some(group) if or_pending => group.push(word),
                _ => groups.push(vec![word]),
            }
            or_pending = false;
        }
        if bytes.get(i) == Some(&b'|') {
            or_pending = true;
        }
    }
    groups
}

fn parse_term(raw: &str, now: Now) -> Result<Term, QueryError> {
    let (negate, mut rest) = match raw.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (mut case, mut path, mut only) = (false, false, None);
    loop {
        if let Some(r) = rest.strip_prefix("case:") {
            case = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("path:") {
            path = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("file:") {
            only = Some(Only::Files);
            rest = r;
        } else if let Some(r) = rest.strip_prefix("folder:") {
            only = Some(Only::Folders);
            rest = r;
        } else {
            break;
        }
    }

    let kind = if let Some(value) = rest.strip_prefix("regex:") {
        Kind::Regex(regex(&value.replace('"', ""), case)?)
    } else if let Some(value) = rest.strip_prefix("ext:") {
        Kind::Ext(
            value
                .replace('"', "")
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(fold_case)
                .collect(),
        )
    } else if let Some(value) = rest.strip_prefix("size:") {
        Kind::Size(parse_size(value).ok_or_else(|| QueryError::Size(value.to_owned()))?)
    } else if let Some(value) = rest.strip_prefix("dm:") {
        Kind::Modified(parse_dm(value, now).ok_or_else(|| QueryError::Date(value.to_owned()))?)
    } else {
        let text = rest.replace('"', "");
        if text.is_empty() {
            Kind::All
        } else if text.contains(['*', '?']) {
            // Wildcards match the whole name, unlike plain text which matches anywhere.
            let mut pattern = String::from("^");
            for c in text.chars() {
                match c {
                    '*' => pattern.push_str(".*"),
                    '?' => pattern.push('.'),
                    _ => pattern.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
                }
            }
            pattern.push('$');
            Kind::Regex(regex(&pattern, case)?)
        } else {
            let needle = if case { text } else { fold_case(&text) };
            Kind::Text(Box::new(Finder::new(&needle).into_owned()))
        }
    };
    Ok(Term {
        raw: raw.to_owned(),
        negate,
        case,
        path,
        only,
        kind,
    })
}

fn fold_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    fold_case_into(&mut out, s);
    out
}

fn regex(pattern: &str, case: bool) -> Result<Regex, QueryError> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case)
        .build()
        .map_err(|source| QueryError::Regex {
            pattern: pattern.to_owned(),
            source,
        })
}

/// Accepts an optional comparison (`>`, `>=`, `<`, `<=`, `=`) and a size in
/// b, kb, mb, gb or tb with 1024-based units.
fn parse_size(value: &str) -> Option<RangeInclusive<u64>> {
    let (op, amount) = [">=", "<=", ">", "<", "="]
        .into_iter()
        .find_map(|op| value.strip_prefix(op).map(|rest| (op, rest)))
        .unwrap_or(("=", value));
    let amount = amount.to_ascii_lowercase();
    let digits = amount
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(amount.len());
    let unit: u64 = match &amount[digits..] {
        "" | "b" => 1,
        "kb" => 1 << 10,
        "mb" => 1 << 20,
        "gb" => 1 << 30,
        "tb" => 1 << 40,
        _ => return None,
    };
    let n = amount[..digits].parse::<u64>().ok()?.checked_mul(unit)?;
    Some(match op {
        ">" => n.saturating_add(1)..=u64::MAX,
        ">=" => n..=u64::MAX,
        // No size is below 0, so `size:<0` is rejected instead of silently matching nothing.
        "<" => 0..=n.checked_sub(1)?,
        "<=" => 0..=n,
        _ => n..=n,
    })
}

/// Days are local calendar days, so `dm:today` follows the user's clock.
fn parse_dm(value: &str, now: Now) -> Option<Range<i64>> {
    let (first, last) = value.split_once("..").unwrap_or((value, value));
    let start = local_day(first, now)? * DAY - now.utc_offset;
    let end = (local_day(last, now)? + 1) * DAY - now.utc_offset;
    Some(start..end)
}

/// Days since 1970-01-01 for `today`, `yesterday` or `YYYY-MM-DD`.
fn local_day(value: &str, now: Now) -> Option<i64> {
    let today = (now.unix + now.utc_offset).div_euclid(DAY);
    match value {
        "today" => return Some(today),
        "yesterday" => return Some(today - 1),
        _ => {}
    }
    let mut parts = value.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }
    // Howard Hinnant's days_from_civil with years starting in March.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}
