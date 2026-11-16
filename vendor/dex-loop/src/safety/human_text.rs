//! What a message Dex starts on its own may say to a person.
//!
//! A scheduled check, a watch or a follow-up has no question a person typed,
//! so nobody asked for record field names or machine timestamps. Dex reads
//! owner records whose fields are `waiting_since` and whose times are RFC 3339
//! with microseconds, and a model that writes from them can echo both
//! ("no items with a waiting_since timestamp after
//! 2026-11-11T15:06:46.833055Z"). [`machine_terms`] finds them and
//! [`humanize`] rewrites them deterministically before the message is
//! posted: a timestamp becomes "Nov 11, 15:06 UTC" (the notices' as-of
//! style) and a snake_case field name becomes words ("waiting since").
//!
//! A code span holding only a field name or a timestamp (`` `waiting_since` ``,
//! as the posted needs-you answer wrote it) is prose to the person and is
//! rewritten without its backticks. Other code spans, fenced blocks, Slack
//! links (`<url|label>`), URLs, paths, email addresses and emoji shortcodes
//! are left exactly as they are. A
//! reply to a person's own question is not rewritten: they may have asked
//! about a field by name.

/// One field name or machine timestamp in human-facing text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineTerm {
    /// A snake_case identifier such as `waiting_since`.
    FieldName(String),
    /// An RFC 3339 timestamp such as `2026-11-11T15:06:46.833055Z`.
    Timestamp(String),
}

/// Every field name and machine timestamp in prose, in order.
pub fn machine_terms(text: &str) -> Vec<MachineTerm> {
    let mut found = Vec::new();
    for_each_prose_word(text, |word| {
        let (_, core, _) = trim_word(word);
        if let Some((stamp, _)) = timestamp_prefix(core) {
            found.push(MachineTerm::Timestamp(stamp.to_owned()));
        } else if is_field_name(core) {
            found.push(MachineTerm::FieldName(core.to_owned()));
        }
        None
    });
    found
}

/// `text` with each machine timestamp in the notices' as-of style and each
/// field name as words; everything else, including whitespace, unchanged.
pub fn humanize(text: &str) -> String {
    for_each_prose_word(text, |word| {
        let (lead, core, tail) = trim_word(word);
        if let Some((stamp, rest)) = timestamp_prefix(core) {
            let human = as_of(stamp)?;
            Some(format!("{lead}{human}{rest}{tail}"))
        } else if is_field_name(core) {
            Some(format!("{lead}{}{tail}", core.replace('_', " ")))
        } else {
            None
        }
    })
}

/// Calls `rewrite` on each whitespace-separated word outside code spans,
/// fenced blocks and Slack angle-bracket tokens, and rebuilds the text with
/// its replacement (or the word itself on `None`).
fn for_each_prose_word(text: &str, mut rewrite: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        out.push_str(&rewrite_line(line, &mut rewrite));
    }
    out
}

/// One line: `<...>` tokens and backtick spans are copied verbatim, except a
/// span that is only a field name or a timestamp, which goes through
/// `rewrite` as a word; the words between them go through `rewrite`.
fn rewrite_line(line: &str, rewrite: &mut impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let next = rest.find(['`', '<']);
        let Some(start) = next else {
            out.push_str(&rewrite_words(rest, rewrite));
            return out;
        };
        out.push_str(&rewrite_words(&rest[..start], rewrite));
        let close = if rest[start..].starts_with('`') {
            '`'
        } else {
            '>'
        };
        match rest[start + 1..].find(close) {
            Some(len) => {
                let end = start + 1 + len + 1;
                let inner = &rest[start + 1..start + 1 + len];
                // A code span holding only a field name or a timestamp is
                // still that value in prose: Dex writes `waiting_since` in
                // backticks, and the person reads it the same.
                let replaced = (close == '`' && is_machine_value(inner))
                    .then(|| rewrite(inner))
                    .flatten();
                out.push_str(replaced.as_deref().unwrap_or(&rest[start..end]));
                rest = &rest[end..];
            }
            None => {
                out.push_str(&rewrite_words(&rest[start..], rewrite));
                return out;
            }
        }
    }
}

fn rewrite_words(segment: &str, rewrite: &mut impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut word_start: Option<usize> = None;
    for (index, c) in segment.char_indices() {
        if c.is_whitespace() {
            if let Some(start) = word_start.take() {
                let word = &segment[start..index];
                out.push_str(&rewrite(word).unwrap_or_else(|| word.to_owned()));
            }
            out.push(c);
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }
    if let Some(start) = word_start {
        let word = &segment[start..];
        out.push_str(&rewrite(word).unwrap_or_else(|| word.to_owned()));
    }
    out
}

/// Splits leading and trailing punctuation off a word: `("(", "x_y", "),")`.
fn trim_word(word: &str) -> (&str, &str, &str) {
    let is_edge = |c: char| {
        matches!(
            c,
            '(' | ')' | '[' | ']' | '"' | '\'' | '“' | '”' | ',' | ';' | '!' | '?' | '*'
        )
    };
    let core_start = word.find(|c: char| !is_edge(c)).unwrap_or(word.len());
    let lead = &word[..core_start];
    let rest = &word[core_start..];
    let mut core = rest.trim_end_matches(is_edge);
    // A sentence-final period is punctuation; one inside a word is not.
    if core.ends_with('.') && !core[..core.len() - 1].contains('.') {
        core = &core[..core.len() - 1];
    }
    (lead, core, &rest[core.len()..])
}

/// A record field name as written in read data: lowercase ASCII words joined
/// by underscores (`waiting_since`, `review_path`), two to four of them.
/// Anything with a digit, a capital, a dot, a slash, a colon or an `@` is a
/// name, version, path, URL, email address or emoji shortcode, and is kept.
pub(crate) fn is_field_name(core: &str) -> bool {
    let parts: Vec<&str> = core.split('_').collect();
    (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase()))
}

/// The whole of `inner` is one field name or one timestamp.
fn is_machine_value(inner: &str) -> bool {
    is_field_name(inner) || timestamp_prefix(inner).is_some_and(|(_, rest)| rest.is_empty())
}

/// An RFC 3339 timestamp at the start of `core`, and what follows it.
fn timestamp_prefix(core: &str) -> Option<(&str, &str)> {
    let bytes = core.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range.clone())
            .is_some_and(|b| b.iter().all(u8::is_ascii_digit))
    };
    let at = |index: usize, c: u8| bytes.get(index) == Some(&c);
    if !(digits(0..4)
        && at(4, b'-')
        && digits(5..7)
        && at(7, b'-')
        && digits(8..10)
        && (at(10, b'T') || at(10, b't'))
        && digits(11..13)
        && at(13, b':')
        && digits(14..16))
    {
        return None;
    }
    let mut end = 16;
    if at(end, b':') && digits(end + 1..end + 3) {
        end += 3;
        if at(end, b'.') {
            end += 1;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
        }
    }
    if at(end, b'Z') || at(end, b'z') {
        end += 1;
    } else if (at(end, b'+') || at(end, b'-'))
        && digits(end + 1..end + 3)
        && at(end + 3, b':')
        && digits(end + 4..end + 6)
    {
        end += 6;
    } else {
        return None;
    }
    Some((&core[..end], &core[end..]))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "Nov 11, 15:06 UTC" from an RFC 3339 timestamp, in UTC. `None` when a
/// field is out of range, so a malformed value is left as written.
fn as_of(stamp: &str) -> Option<String> {
    let num = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute) = (num(11..13)?, num(14..16)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let offset_minutes = match stamp.as_bytes().last()? {
        b'Z' | b'z' => 0,
        _ => {
            let tail = stamp.get(stamp.len() - 6..)?;
            let sign = if tail.starts_with('-') { -1 } else { 1 };
            let hours: i64 = tail.get(1..3)?.parse().ok()?;
            let minutes: i64 = tail.get(4..6)?.parse().ok()?;
            sign * (hours * 60 + minutes)
        }
    };
    let total = days_from_civil(year, month, day) * 1440 + hour * 60 + minute - offset_minutes;
    let (days, minute_of_day) = (total.div_euclid(1440), total.rem_euclid(1440));
    let (_, month, day) = civil_from_days(days);
    let name = MONTHS.get(usize::try_from(month - 1).ok()?)?;
    Some(format!(
        "{name} {day}, {:02}:{:02} UTC",
        minute_of_day / 60,
        minute_of_day % 60
    ))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer Dex posted to the founder's DM from the scheduled
    /// needs-you check. Its instruction handed it the field name and the
    /// raw window start, and it repeated both.
    const POSTED: &str = "There are no items with a waiting_since timestamp after \
        2026-11-11T15:06:46.833055Z.\n(The single questionnaire assignment has a waiting_since \
        timestamp of 2026-11-08T05:48:31Z, which is before your cutoff, and the 6 training \
        courses do not provide a waiting_since timestamp in the read data.)";

    #[test]
    fn the_posted_needs_you_answer_is_caught_and_rewritten_for_a_person() {
        assert_eq!(
            machine_terms(POSTED),
            [
                MachineTerm::FieldName("waiting_since".into()),
                MachineTerm::Timestamp("2026-11-11T15:06:46.833055Z".into()),
                MachineTerm::FieldName("waiting_since".into()),
                MachineTerm::Timestamp("2026-11-08T05:48:31Z".into()),
                MachineTerm::FieldName("waiting_since".into()),
            ]
        );
        let human = humanize(POSTED);
        assert_eq!(
            human,
            "There are no items with a waiting since timestamp after Nov 11, 15:06 UTC.\n\
             (The single questionnaire assignment has a waiting since timestamp of Nov 8, \
             05:48 UTC, which is before your cutoff, and the 6 training courses do not provide \
             a waiting since timestamp in the read data.)"
        );
        assert_eq!(machine_terms(&human), []);
    }

    /// The same answer exactly as Slack showed it on Nov 12: Dex put the
    /// field name in backticks, which the first version of the guard took
    /// for code and posted unchanged.
    const POSTED_IN_CODE_SPANS: &str = "There are no items with a `waiting_since` timestamp \
        after 2026-11-11T15:06:46.833055Z.\n\n(The single questionnaire assignment has a \
        `waiting_since` timestamp of 2026-11-08T05:48:31Z, which is before your cutoff, and the \
        6 training courses do not provide a `waiting_since` timestamp in the read data.)";

    #[test]
    fn a_field_name_in_backticks_is_still_a_field_name_to_the_person() {
        assert_eq!(machine_terms(POSTED_IN_CODE_SPANS).len(), 5);
        let human = humanize(POSTED_IN_CODE_SPANS);
        assert_eq!(
            human,
            "There are no items with a waiting since timestamp after Nov 11, 15:06 UTC.\n\n\
             (The single questionnaire assignment has a waiting since timestamp of Nov 8, \
             05:48 UTC, which is before your cutoff, and the 6 training courses do not provide \
             a waiting since timestamp in the read data.)"
        );
        assert_eq!(machine_terms(&human), []);
        // An error code and a timestamp in backticks, as a Dex-written alarm
        // wrote them.
        assert_eq!(
            humanize("failing with `storage_unavailable` since `2026-11-09T12:09:00Z`."),
            "failing with storage unavailable since Nov 9, 12:09 UTC."
        );
    }

    #[test]
    fn offsets_are_converted_to_utc_across_day_boundaries() {
        assert_eq!(
            humanize("since 2026-12-31T23:30:00-01:00."),
            "since Jan 1, 00:30 UTC."
        );
        assert_eq!(
            humanize("at 2026-03-01T00:10+02:00"),
            "at Feb 28, 22:10 UTC"
        );
        assert_eq!(humanize("(2024-02-29t12:00:00z)"), "(Feb 29, 12:00 UTC)");
        // Not a timestamp, or out of range: left as written.
        for kept in [
            "2026-11-11",
            "2026-11-11T15:06",
            "2026-13-01T00:00:00Z",
            "v2026-11-11T15:06Z",
        ] {
            assert_eq!(humanize(kept), kept);
        }
    }

    #[test]
    fn code_links_paths_names_and_emoji_are_left_alone() {
        for kept in [
            "Run `kubectl get pods` in the console.",
            "Set `max_retries: 3` there.",
            "```\nwaiting_since: 2026-11-08T05:48:31Z\n```",
            "See <https://app.deixic.com/x?sort=waiting_since|the waiting_since list>.",
            "Open https://app.deixic.com/needs_you now.",
            "Edit src/needs_you_digest.rs today.",
            "Mail ops_team@example.com about it.",
            "Done :white_check_mark:",
            "The S3 bucket logs_2024 and Prod_Data stay as named.",
            "A single_word_with_too_many_parts_here stays.",
        ] {
            assert_eq!(humanize(kept), kept, "{kept}");
            assert_eq!(machine_terms(kept), [], "{kept}");
        }
    }

    #[test]
    fn plain_prose_and_whitespace_are_unchanged() {
        let text = "One questionnaire is waiting on you.\n\n  - since Nov 8\n";
        assert_eq!(humanize(text), text);
        assert_eq!(humanize("**review_path**, then"), "**review path**, then");
    }
}
