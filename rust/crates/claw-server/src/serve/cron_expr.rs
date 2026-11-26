//! Phase 6:最小 cron 表达式解析(分 时 日 月 周)。
//!
//! 只覆盖 Web 任务面板需要的语法:
//! - `*`、`*/n`、`a`、`a-b`、`a-b/n`、`a/n`,以及用逗号分隔的组合;
//! - 周字段 0 与 7 都表示周日;
//! - 日与周同时受限时采用标准 cron 的「或」语义。
//!
//! 时间按本地时区解释:调用方传 [`chrono::DateTime<Local>`],解析层不处理时区。

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, TimeZone, Timelike};

/// 最远向后扫描 5 年:覆盖 `0 0 29 2 *`(闰日)这类低频表达式。
const MAX_SCAN_MINUTES: i64 = 60 * 24 * 366 * 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: [bool; 60],
    hours: [bool; 24],
    // 日/月按 1 起编号,位图留出首尾空槽以直接用数值做下标。
    days: [bool; 32],
    months: [bool; 13],
    weekdays: [bool; 7],
    /// 日/周字段是否为 `*`(用于决定两者的组合语义)。
    dom_any: bool,
    dow_any: bool,
}

impl CronSchedule {
    /// 解析 5 段 cron 表达式;格式错误时返回中文可读原因。
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron 表达式需要 5 段(分 时 日 月 周),实际 {} 段",
                fields.len()
            ));
        }
        let minutes = parse_field::<60>(fields[0], "分", 0, 59)?;
        let hours = parse_field::<24>(fields[1], "时", 0, 23)?;
        let days = parse_field::<32>(fields[2], "日", 1, 31)?;
        let months = parse_field::<13>(fields[3], "月", 1, 12)?;
        let dow_raw = parse_field::<8>(fields[4], "周", 0, 7)?;
        // 7 与 0 同义(都表示周日),折进 0 后再存。
        let mut weekdays = [false; 7];
        weekdays.copy_from_slice(&dow_raw[..7]);
        if dow_raw[7] {
            weekdays[0] = true;
        }
        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays,
            dom_any: fields[2].trim() == "*",
            dow_any: fields[4].trim() == "*",
        })
    }

    /// 给定本地时间是否命中该表达式(精确到分钟)。
    pub fn matches(&self, at: &DateTime<Local>) -> bool {
        if !self.minutes[at.minute() as usize]
            || !self.hours[at.hour() as usize]
            || !self.months[at.month() as usize]
        {
            return false;
        }
        let dom = self.days[at.day() as usize];
        let dow = self.weekdays[at.weekday().num_days_from_sunday() as usize];
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            // 标准 cron:日与周都受限时任一命中即可。
            (false, false) => dom || dow,
        }
    }

    /// 严格晚于 `after` 的下一个命中时间(按分钟对齐)。
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut cursor = after
            .with_second(0)?
            .with_nanosecond(0)?
            .checked_add_signed(ChronoDuration::minutes(1))?;
        for _ in 0..MAX_SCAN_MINUTES {
            if self.matches(&cursor) {
                return Some(cursor);
            }
            cursor = cursor.checked_add_signed(ChronoDuration::minutes(1))?;
        }
        None
    }

    /// 以毫秒时间戳为基准求下一次执行毫秒时间戳。
    pub fn next_after_ms(&self, after_ms: u64) -> Option<u64> {
        let millis = i64::try_from(after_ms).ok()?;
        let after = Local.timestamp_millis_opt(millis).single()?;
        self.next_after(after)
            .and_then(|at| u64::try_from(at.timestamp_millis()).ok())
    }
}

/// 解析单个字段到布尔位图。`name` 只用于错误提示。
fn parse_field<const N: usize>(
    field: &str,
    name: &str,
    min: u32,
    max: u32,
) -> Result<[bool; N], String> {
    let mut bits = [false; N];
    let max_index = usize::try_from(max).map_err(|_| format!("{name}字段范围无效"))?;
    if max_index >= N {
        return Err(format!("{name}字段范围超出位图容量"));
    }
    for item in field.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(format!("{name}字段包含空项"));
        }
        let (range_part, step) = match item.split_once('/') {
            Some((range_part, raw_step)) => {
                let step = raw_step
                    .parse::<u32>()
                    .map_err(|_| format!("{name}字段步长无效:{raw_step}"))?;
                if step == 0 {
                    return Err(format!("{name}字段步长不能为 0"));
                }
                (range_part.trim(), step)
            }
            None => (item, 1),
        };
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((raw_start, raw_end)) = range_part.split_once('-') {
            (
                parse_number(raw_start, name, min, max)?,
                parse_number(raw_end, name, min, max)?,
            )
        } else {
            let value = parse_number(range_part, name, min, max)?;
            // `a/n` 表示从 a 到字段上限,步长 n;`a` 表示只匹配 a。
            if step == 1 {
                (value, value)
            } else {
                (value, max)
            }
        };
        if start > end {
            return Err(format!("{name}字段范围起点大于终点:{range_part}"));
        }
        let mut value = start;
        while value <= end {
            bits[value as usize] = true;
            value += step;
        }
    }
    Ok(bits)
}

fn parse_number(raw: &str, name: &str, min: u32, max: u32) -> Result<u32, String> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("{name}字段包含无效值:{raw}"));
    }
    let value = raw
        .parse::<u32>()
        .map_err(|_| format!("{name}字段数值溢出:{raw}"))?;
    if value < min || value > max {
        return Err(format!("{name}字段 {value} 超出范围 {min}-{max}"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid local time")
    }

    #[test]
    fn parses_every_minute_and_computes_next_minute() {
        let schedule = CronSchedule::parse("* * * * *").expect("parse");
        assert_eq!(
            schedule.next_after(local(2026, 1, 1, 12, 30)),
            Some(local(2026, 1, 1, 12, 31))
        );
    }

    #[test]
    fn parses_steps_lists_and_ranges() {
        let schedule = CronSchedule::parse("*/15 9-18 * * 1-5").expect("parse");
        assert!(schedule.matches(&local(2026, 1, 5, 9, 0))); // 周一 9:00
        assert!(schedule.matches(&local(2026, 1, 5, 9, 45)));
        assert!(!schedule.matches(&local(2026, 1, 5, 9, 10)));
        assert!(!schedule.matches(&local(2026, 1, 4, 9, 0))); // 周日
        assert!(!schedule.matches(&local(2026, 1, 5, 19, 0)));

        let schedule = CronSchedule::parse("0 0 1,15 * *").expect("parse");
        assert!(schedule.matches(&local(2026, 1, 1, 0, 0)));
        assert!(schedule.matches(&local(2026, 1, 15, 0, 0)));
        assert!(!schedule.matches(&local(2026, 1, 2, 0, 0)));
    }

    #[test]
    fn weekday_seven_aliases_sunday() {
        let schedule = CronSchedule::parse("0 9 * * 7").expect("parse");
        assert!(schedule.matches(&local(2026, 1, 4, 9, 0))); // 周日
        assert!(!schedule.matches(&local(2026, 1, 5, 9, 0)));
    }

    #[test]
    fn dom_and_dow_use_or_semantics_when_both_restricted() {
        // 每月 1 号或每周一。
        let schedule = CronSchedule::parse("0 8 1 * 1").expect("parse");
        assert!(schedule.matches(&local(2026, 1, 1, 8, 0))); // 1 号(周四)
        assert!(schedule.matches(&local(2026, 1, 5, 8, 0))); // 周一
        assert!(!schedule.matches(&local(2026, 1, 6, 8, 0)));
    }

    #[test]
    fn next_after_crosses_day_boundary() {
        let schedule = CronSchedule::parse("0 9 * * *").expect("parse");
        let next = schedule.next_after(local(2026, 1, 1, 9, 0)).expect("next");
        assert_eq!(next, local(2026, 1, 2, 9, 0));
    }

    #[test]
    fn rejects_invalid_expressions() {
        assert!(CronSchedule::parse("* * * *").is_err());
        assert!(CronSchedule::parse("* * * * * *").is_err());
        assert!(CronSchedule::parse("60 * * * *").is_err());
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
        assert!(CronSchedule::parse("a * * * *").is_err());
        assert!(CronSchedule::parse("5-1 * * * *").is_err());
    }

    #[test]
    fn next_after_ms_round_trips_local_time() {
        let schedule = CronSchedule::parse("* * * * *").expect("parse");
        let base = local(2026, 1, 1, 12, 30).timestamp_millis() as u64;
        let next = schedule.next_after_ms(base).expect("next");
        assert_eq!(next, local(2026, 1, 1, 12, 31).timestamp_millis() as u64);
    }
}
