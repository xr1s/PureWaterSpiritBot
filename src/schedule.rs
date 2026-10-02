//! 管理员的分类和发布计划，以及它们必须满足的规则。

use std::{collections::HashSet, num::NonZeroU16, str::FromStr};

use anyhow::{Context, Result, bail, ensure};
use jiff::{SignedDuration, civil::Time, tz::TimeZone};
use serde::Deserialize;
use teloxide::types::UserId;

use crate::model::{Category, CategoryId, PositiveDuration, Slot};

/// 管理员自己的设置未设置时回退使用的计划设置。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleDefaults {
    #[serde(with = "jiff::fmt::serde::tz::required")]
    pub timezone: TimeZone,
    pub misfire_grace: PositiveDuration,
    pub send_interval: PositiveDuration,
}

/// 调度器需要了解的某位管理员的全部信息。
#[derive(Debug, Clone)]
pub struct AdminSchedule {
    pub admin: UserId,
    pub timezone: TimeZone,
    pub misfire_grace: PositiveDuration,
    pub send_interval: PositiveDuration,
    pub reminder: Option<Time>,
    pub categories: Vec<Category>,
    pub slots: Vec<Slot>,
}

impl AdminSchedule {
    pub fn validate(&self) -> Result<()> {
        validate_categories(&self.categories)?;
        validate_slots(&self.categories, &self.slots)
    }
}

fn validate_categories(categories: &[Category]) -> Result<()> {
    let mut labels = HashSet::new();
    for category in categories {
        check_label(&category.label)?;
        ensure!(
            labels.insert(&category.label),
            "duplicate category label: {}",
            category.label
        );
    }
    Ok(())
}

fn validate_slots(categories: &[Category], slots: &[Slot]) -> Result<()> {
    let mut times = HashSet::new();
    for slot in slots {
        ensure!(
            times.insert(slot.time),
            "multiple slots use the same time: {}",
            slot.time
        );
        validate_picks(categories, slot)
            .with_context(|| format!("invalid slot at {}", format_clock(slot.time)))?;
    }
    Ok(())
}

/// 没有配额的时段是合法的：刚新建的时段在添加配额前就是空的，调度时会被跳过。
fn validate_picks(categories: &[Category], slot: &Slot) -> Result<()> {
    let mut picked = HashSet::new();
    for pick in &slot.picks {
        require_category(categories, pick.category)?;
        ensure!(
            picked.insert(&pick.category),
            "duplicate pick category {}",
            pick.category.0
        );
        let mut fallbacks = HashSet::new();
        for fallback in &pick.fallback {
            require_category(categories, *fallback)?;
            ensure!(
                *fallback != pick.category,
                "fallback must not contain its own category {}",
                fallback.0
            );
            ensure!(
                fallbacks.insert(fallback),
                "duplicate fallback category {}",
                fallback.0
            );
        }
    }
    Ok(())
}

fn check_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty() && label.trim() == label && !label.chars().any(char::is_control),
        "category label {label:?} must be non-empty, trimmed and free of control characters"
    );
    Ok(())
}

fn require_category(categories: &[Category], id: CategoryId) -> Result<()> {
    if !categories.iter().any(|category| category.id == id) {
        bail!("unknown category {}", id.0);
    }
    Ok(())
}

/// 分类名称的最大字符数，名称会显示在按钮上。
const MAX_LABEL_CHARS: usize = 24;
/// 单个配额最多取多少条。
const MAX_PICK_COUNT: u16 = 99;

/// 解析用户输入的分类名称，返回去掉首尾空白后的名称。
pub fn parse_label(input: &str) -> Result<String> {
    let label = input.trim();
    ensure!(!label.is_empty(), "名称不能为空");
    ensure!(
        !label.chars().any(char::is_control),
        "名称不能包含换行等控制字符"
    );
    ensure!(
        label.chars().count() <= MAX_LABEL_CHARS,
        "名称最多 {MAX_LABEL_CHARS} 个字符"
    );
    check_label(label)?;
    Ok(label.to_owned())
}

/// 拒绝或撤下投稿时写的理由最多多少个字符。
const MAX_REASON_CHARS: usize = 300;

/// 解析管理员写的理由，返回去掉首尾空白后的文字。
pub fn parse_reason(input: &str) -> Result<String> {
    let reason = input.trim();
    ensure!(!reason.is_empty(), "理由不能为空");
    ensure!(
        reason.chars().count() <= MAX_REASON_CHARS,
        "理由最多 {MAX_REASON_CHARS} 个字符"
    );
    Ok(reason.to_owned())
}

/// 解析配额数量，范围是 1 到 99。
pub fn parse_count(input: &str) -> Result<NonZeroU16> {
    let count: u16 = input.trim().parse().context("数量必须是整数")?;
    ensure!(
        (1..=MAX_PICK_COUNT).contains(&count),
        "数量必须在 1 到 {MAX_PICK_COUNT} 之间"
    );
    Ok(NonZeroU16::new(count).expect("count was checked to be at least 1"))
}

/// 解析 `H:MM` 或 `HH:MM` 形式的时刻，也接受全角冒号。
pub fn parse_clock(input: &str) -> Result<Time> {
    let text = input.trim().replace('：', ":");
    let (hour, minute) = text.split_once(':').context("时间格式应为 HH:MM")?;
    let digits = |part: &str| part.len() <= 2 && part.bytes().all(|byte| byte.is_ascii_digit());
    ensure!(digits(hour) && digits(minute), "时间格式应为 HH:MM");
    let hour: i8 = hour.parse().context("时间格式应为 HH:MM")?;
    let minute: i8 = minute.parse().context("时间格式应为 HH:MM")?;
    Time::new(hour, minute, 0, 0).map_err(|_| anyhow::anyhow!("时间必须在 00:00 到 23:59 之间"))
}

/// 把时刻写成补零的 `HH:MM`，数据库和界面都用这种形式。
pub fn format_clock(time: Time) -> String {
    format!("{:02}:{:02}", time.hour(), time.minute())
}

/// 解析 IANA 时区名，返回规范的名称。
pub fn parse_timezone(input: &str) -> Result<String> {
    let zone = TimeZone::get(input.trim())
        .map_err(|_| anyhow::anyhow!("无法识别的时区，请使用 IANA 名称，例如 Asia/Shanghai"))?;
    let name = zone
        .iana_name()
        .context("无法识别的时区，请使用 IANA 名称，例如 Asia/Shanghai")?;
    Ok(name.to_owned())
}

fn parse_bounded_duration(
    input: &str,
    min: SignedDuration,
    max: SignedDuration,
    range: &str,
) -> Result<PositiveDuration> {
    let duration = SignedDuration::from_str(input.trim())
        .map_err(|_| anyhow::anyhow!("时长格式不对，例如 30m、2h、3s"))?;
    ensure!((min..=max).contains(&duration), "时长必须在 {range} 之间");
    PositiveDuration::try_from(duration)
}

/// 解析错过时段后仍可补发的宽限期，范围是 1 分钟到 24 小时。
pub fn parse_misfire_grace(input: &str) -> Result<PositiveDuration> {
    parse_bounded_duration(
        input,
        SignedDuration::from_mins(1),
        SignedDuration::from_hours(24),
        "1 分钟和 24 小时",
    )
}

/// 解析同一时段内相邻两条投稿的发送间隔，范围是 1 秒到 10 分钟。
pub fn parse_send_interval(input: &str) -> Result<PositiveDuration> {
    parse_bounded_duration(
        input,
        SignedDuration::from_secs(1),
        SignedDuration::from_mins(10),
        "1 秒和 10 分钟",
    )
}

/// 把时长写成 `2h`、`30m`、`3s` 这样的简短形式，数据库和界面都用它。
pub fn format_duration(duration: PositiveDuration) -> String {
    format!("{:#}", duration.signed())
}

#[cfg(test)]
pub mod tests {
    use std::num::NonZeroU16;

    use jiff::Timestamp;

    use super::*;
    use crate::model::{Pick, PickId, SlotId};

    /// 测试里用名字指代分类，按固定表映射成 ID。
    /// 这张表和 `test_support::seed` 的插入顺序一致，所以也对得上数据库里的 ID。
    pub fn cid(name: &str) -> CategoryId {
        CategoryId(match name {
            "gi" => 1,
            "hsr" => 2,
            "other" => 3,
            "misc" => 4,
            "nope" => 99,
            _ => panic!("unknown test category {name}"),
        })
    }

    pub fn category(id: i64, label: &str) -> Category {
        Category {
            id: CategoryId(id),
            label: label.to_owned(),
        }
    }

    pub fn pick(category: &str, count: u16, fallback: &[&str]) -> Pick {
        Pick {
            id: PickId(0),
            category: cid(category),
            count: NonZeroU16::new(count).unwrap(),
            fallback: fallback.iter().map(|value| cid(value)).collect(),
        }
    }

    pub fn slot(id: i64, time: &str, picks: Vec<Pick>) -> Slot {
        Slot {
            id: SlotId(id),
            time: time.parse().unwrap(),
            effective_from: Timestamp::UNIX_EPOCH,
            picks,
        }
    }

    fn schedule(categories: Vec<Category>, slots: Vec<Slot>) -> AdminSchedule {
        AdminSchedule {
            admin: UserId(1),
            timezone: TimeZone::UTC,
            misfire_grace: PositiveDuration::try_from(jiff::SignedDuration::from_hours(2)).unwrap(),
            send_interval: PositiveDuration::try_from(jiff::SignedDuration::from_secs(3)).unwrap(),
            reminder: None,
            categories,
            slots,
        }
    }

    fn categories() -> Vec<Category> {
        vec![category(1, "GI"), category(2, "HSR"), category(3, "Other")]
    }

    #[test]
    fn reasons_are_trimmed_and_limited() {
        assert_eq!(parse_reason("  画质太差\n").unwrap(), "画质太差");
        assert!(parse_reason("  \n ").is_err());
        assert!(parse_reason(&"字".repeat(300)).is_ok());
        assert!(parse_reason(&"字".repeat(301)).is_err());
    }

    #[test]
    fn accepts_an_empty_schedule() {
        assert!(schedule(Vec::new(), Vec::new()).validate().is_ok());
    }

    #[test]
    fn accepts_a_valid_schedule() {
        let slots = vec![
            slot(
                1,
                "10:00",
                vec![pick("gi", 2, &["other", "hsr"]), pick("hsr", 1, &[])],
            ),
            slot(2, "20:00", vec![pick("other", 1, &[])]),
        ];
        assert!(schedule(categories(), slots).validate().is_ok());
    }

    #[test]
    fn rejects_duplicate_category_labels() {
        let mut duplicate_label = categories();
        duplicate_label.push(category(4, "GI"));
        assert!(schedule(duplicate_label, Vec::new()).validate().is_err());
    }

    #[test]
    fn rejects_untrimmed_labels() {
        let categories = vec![category(1, " GI")];
        assert!(schedule(categories, Vec::new()).validate().is_err());
    }

    #[test]
    fn rejects_duplicate_fallback() {
        let slots = vec![slot(1, "10:00", vec![pick("gi", 1, &["other", "other"])])];
        assert!(schedule(categories(), slots).validate().is_err());
    }

    #[test]
    fn rejects_unknown_category_reference() {
        let slots = vec![slot(1, "10:00", vec![pick("nope", 1, &[])])];
        assert!(schedule(categories(), slots).validate().is_err());
    }

    #[test]
    fn rejects_slots_sharing_a_time() {
        let same_time = vec![
            slot(1, "10:00", vec![pick("gi", 1, &[])]),
            slot(2, "10:00", vec![pick("gi", 1, &[])]),
        ];
        assert!(schedule(categories(), same_time).validate().is_err());
    }

    #[test]
    fn accepts_a_slot_without_picks() {
        let slots = vec![slot(1, "10:00", Vec::new())];
        assert!(schedule(categories(), slots).validate().is_ok());
    }

    #[test]
    fn parses_labels() {
        assert_eq!(parse_label("  原神 ").unwrap(), "原神");
        assert!(parse_label("   ").is_err());
        assert!(parse_label("a\nb").is_err());
        assert!(parse_label(&"长".repeat(24)).is_ok());
        assert!(parse_label(&"长".repeat(25)).is_err());
    }

    #[test]
    fn parses_counts() {
        assert_eq!(parse_count(" 3 ").unwrap().get(), 3);
        assert_eq!(parse_count("99").unwrap().get(), 99);
        for bad in ["0", "100", "-1", "x", "", "1.5"] {
            assert!(parse_count(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_clock_times() {
        assert_eq!(format_clock(parse_clock("9:05").unwrap()), "09:05");
        assert_eq!(format_clock(parse_clock("23:59").unwrap()), "23:59");
        assert_eq!(format_clock(parse_clock("08：30").unwrap()), "08:30");
        for bad in ["24:00", "10:60", "10", "1000", "10:5x", "::", "100:00", ""] {
            assert!(parse_clock(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_timezones_to_their_canonical_name() {
        assert_eq!(parse_timezone("Asia/Shanghai").unwrap(), "Asia/Shanghai");
        assert_eq!(parse_timezone("asia/shanghai").unwrap(), "Asia/Shanghai");
        assert!(parse_timezone("Nowhere/Land").is_err());
        assert!(parse_timezone("").is_err());
    }

    #[test]
    fn parses_durations_within_bounds() {
        assert_eq!(format_duration(parse_misfire_grace("2h").unwrap()), "2h");
        assert_eq!(
            format_duration(parse_misfire_grace("90m").unwrap()),
            "1h 30m"
        );
        assert!(parse_misfire_grace("30s").is_err());
        assert!(parse_misfire_grace("25h").is_err());
        assert!(parse_misfire_grace("soon").is_err());
        assert_eq!(format_duration(parse_send_interval("3s").unwrap()), "3s");
        assert!(parse_send_interval("0s").is_err());
        assert!(parse_send_interval("11m").is_err());
    }
}
