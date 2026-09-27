//! RFC 7231 "HTTP-date" formatting for the `Last-Modified` header — distinct from the
//! RFC 3339 timestamps used in XML bodies (§59/§60). Standard clients/SDKs parse this header
//! unconditionally on GetObject/HeadObject responses, so its absence is a hard
//! compatibility break, not a cosmetic one.

use time::OffsetDateTime;

pub fn http_date(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    let weekday = match t.weekday() {
        time::Weekday::Monday => "Mon",
        time::Weekday::Tuesday => "Tue",
        time::Weekday::Wednesday => "Wed",
        time::Weekday::Thursday => "Thu",
        time::Weekday::Friday => "Fri",
        time::Weekday::Saturday => "Sat",
        time::Weekday::Sunday => "Sun",
    };
    let month = match t.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    format!(
        "{weekday}, {:02} {month} {} {:02}:{:02}:{:02} GMT",
        t.day(),
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn formats_a_known_instant() {
        let t = datetime!(2024-01-15 08:12:31 UTC);
        assert_eq!(http_date(t), "Mon, 15 Jan 2024 08:12:31 GMT");
    }
}
