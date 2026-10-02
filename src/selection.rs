use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use jiff::{Timestamp, civil::Time, tz::TimeZone};

use crate::model::{Candidate, CategoryId, PostId, Slot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub post_id: PostId,
    /// 槽位请求的分类；回退时与帖子自身的分类不同。
    pub configured_category: CategoryId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortage {
    pub category: CategoryId,
    pub missing: u16,
}

#[derive(Debug, Default)]
pub struct Selection {
    pub selected: Vec<Selected>,
    pub shortages: Vec<Shortage>,
}

/// 按分类划分的队列，按发布顺序保存候选 ID。
struct Pool(HashMap<CategoryId, VecDeque<PostId>>);

impl Pool {
    fn new(candidates: &[Candidate]) -> Self {
        let mut queues: HashMap<CategoryId, VecDeque<PostId>> = HashMap::new();
        for candidate in candidates {
            queues
                .entry(candidate.category)
                .or_default()
                .push_back(candidate.id);
        }
        Self(queues)
    }

    fn take(&mut self, category: &CategoryId, limit: usize) -> Vec<PostId> {
        let Some(queue) = self.0.get_mut(category) else {
            return Vec::new();
        };
        let count = limit.min(queue.len());
        queue.drain(..count).collect()
    }
}

/// 为某个槽位挑选帖子。`candidates` 必须已按发布顺序排列。
///
/// 每次挑选都先从自身分类中取，因此回退永远不会占用
/// 同一槽位中后续挑选应得的帖子。只有在此之后，缺口才会由
/// 各次挑选的有序回退列表来补足。
pub fn select_posts(slot: &Slot, candidates: &[Candidate]) -> Selection {
    let mut pool = Pool::new(candidates);
    let mut filled: Vec<Vec<PostId>> = slot
        .picks
        .iter()
        .map(|pick| pool.take(&pick.category, usize::from(pick.count.get())))
        .collect();

    let mut selection = Selection::default();
    for (pick, ids) in slot.picks.iter().zip(&mut filled) {
        for category in &pick.fallback {
            let missing = usize::from(pick.count.get()) - ids.len();
            if missing == 0 {
                break;
            }
            ids.extend(pool.take(category, missing));
        }
        let missing = usize::from(pick.count.get()) - ids.len();
        if missing > 0 {
            selection.shortages.push(Shortage {
                category: pick.category,
                missing: u16::try_from(missing).expect("missing count is bounded by a u16 quota"),
            });
        }
        selection
            .selected
            .extend(ids.iter().map(|&post_id| Selected {
                post_id,
                configured_category: pick.category,
            }));
    }
    selection
}

#[derive(Debug, PartialEq, Eq)]
pub struct SlotForecast {
    pub time: Time,
    pub planned: u16,
    pub selected: usize,
    pub shortages: Vec<Shortage>,
}

/// 预测按当前队列，一整天的槽位会发布什么。
pub fn forecast(slots: &[Slot], candidates: &[Candidate]) -> Vec<SlotForecast> {
    let mut remaining = candidates.to_vec();
    let mut ordered: Vec<&Slot> = slots.iter().collect();
    ordered.sort_by_key(|slot| slot.time);
    ordered
        .into_iter()
        .map(|slot| {
            let selection = select_posts(slot, &remaining);
            let taken: HashSet<PostId> =
                selection.selected.iter().map(|item| item.post_id).collect();
            remaining.retain(|candidate| !taken.contains(&candidate.id));
            SlotForecast {
                time: slot.time,
                planned: slot.planned_count(),
                selected: selection.selected.len(),
                shortages: selection.shortages,
            }
        })
        .collect()
}

/// 发送顺序里的一篇投稿。`at` 是预计发出的时段，`None` 表示按现在的时段永远轮不到它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upcoming {
    pub post: PostId,
    pub at: Option<Timestamp>,
}

/// 按当前队列，从 `now` 之后的第一个时段起一个一个地模拟，排出所有排队投稿的发送顺序。
/// 某一整天一篇都选不出来时，剩下的投稿以后也不会被选中，按原来的顺序排在最后。
pub fn upcoming(
    slots: &[Slot],
    timezone: &TimeZone,
    now: Timestamp,
    candidates: &[Candidate],
) -> Result<Vec<Upcoming>> {
    let mut ordered: Vec<&Slot> = slots.iter().filter(|slot| !slot.picks.is_empty()).collect();
    ordered.sort_by_key(|slot| slot.time);
    let mut remaining = candidates.to_vec();
    let mut order = Vec::with_capacity(candidates.len());
    let mut date = now.to_zoned(timezone.clone()).date();
    let mut today = true;
    while !remaining.is_empty() {
        let mut selected_today = 0;
        for slot in &ordered {
            let at = date
                .to_datetime(slot.time)
                .to_zoned(timezone.clone())?
                .timestamp();
            if at <= now || at < slot.effective_from {
                continue;
            }
            let selection = select_posts(slot, &remaining);
            let taken: HashSet<PostId> =
                selection.selected.iter().map(|item| item.post_id).collect();
            remaining.retain(|candidate| !taken.contains(&candidate.id));
            selected_today += selection.selected.len();
            order.extend(selection.selected.iter().map(|item| Upcoming {
                post: item.post_id,
                at: Some(at),
            }));
        }
        // 今天剩下的时段可能本来就不多，只有完整的一天选不出来才说明以后也轮不到。
        if selected_today == 0 && !today {
            break;
        }
        today = false;
        date = date.tomorrow()?;
    }
    order.extend(remaining.into_iter().map(|candidate| Upcoming {
        post: candidate.id,
        at: None,
    }));
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::Pick,
        schedule::tests::{cid, pick, slot as make_slot},
    };

    fn key(value: &str) -> CategoryId {
        cid(value)
    }

    fn candidate(id: i64, category: &str) -> Candidate {
        Candidate {
            id: PostId(id),
            category: key(category),
        }
    }

    fn slot(picks: Vec<Pick>) -> Slot {
        make_slot(1, "10:00", picks)
    }

    fn ids(selection: &Selection) -> Vec<i64> {
        selection
            .selected
            .iter()
            .map(|item| item.post_id.0)
            .collect()
    }

    #[test]
    fn protects_configured_quotas_before_fallback() {
        let slot = slot(vec![pick("gi", 1, &["hsr"]), pick("hsr", 1, &["other"])]);
        let candidates = [candidate(1, "hsr"), candidate(2, "other")];
        let selection = select_posts(&slot, &candidates);
        assert_eq!(ids(&selection), vec![1]);
        assert_eq!(
            selection.shortages,
            vec![Shortage {
                category: key("gi"),
                missing: 1
            }]
        );
    }

    #[test]
    fn reports_shortage_when_fallback_is_exhausted() {
        let slot = slot(vec![pick("gi", 2, &["hsr"])]);
        let selection = select_posts(&slot, &[candidate(1, "gi")]);
        assert_eq!(ids(&selection), vec![1]);
        assert_eq!(
            selection.shortages,
            vec![Shortage {
                category: key("gi"),
                missing: 1
            }]
        );
    }

    #[test]
    fn fallback_follows_configured_order() {
        let slot = slot(vec![pick("gi", 2, &["other", "hsr"])]);
        let candidates = [
            candidate(1, "hsr"),
            candidate(2, "other"),
            candidate(3, "hsr"),
        ];
        let selection = select_posts(&slot, &candidates);
        assert_eq!(ids(&selection), vec![2, 1]);
        assert!(selection.shortages.is_empty());
    }

    #[test]
    fn forecast_does_not_reuse_posts_across_slots() {
        let first = slot(vec![pick("gi", 1, &[])]);
        let second = make_slot(2, "12:00", vec![pick("gi", 1, &[])]);
        let forecasts = forecast(&[second, first], &[candidate(1, "gi")]);
        assert_eq!(forecasts.len(), 2);
        assert_eq!(forecasts[0].time, "10:00".parse().unwrap());
        assert_eq!(forecasts[0].selected, 1);
        assert_eq!(forecasts[1].selected, 0);
        assert_eq!(forecasts[1].shortages.len(), 1);
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn order(upcoming: &[Upcoming]) -> Vec<(i64, Option<Timestamp>)> {
        upcoming.iter().map(|item| (item.post.0, item.at)).collect()
    }

    #[test]
    fn upcoming_starts_after_now_and_rolls_over_to_the_next_days() {
        let slots = [
            make_slot(1, "18:00", vec![pick("gi", 1, &[])]),
            make_slot(2, "09:00", vec![pick("gi", 1, &[]), pick("hsr", 1, &[])]),
        ];
        let candidates = [
            candidate(1, "gi"),
            candidate(2, "gi"),
            candidate(3, "hsr"),
            candidate(4, "gi"),
        ];
        let now = at("2026-01-01T12:00:00Z");
        let upcoming = upcoming(&slots, &TimeZone::UTC, now, &candidates).unwrap();
        assert_eq!(
            order(&upcoming),
            [
                (1, Some(at("2026-01-01T18:00:00Z"))),
                (2, Some(at("2026-01-02T09:00:00Z"))),
                (3, Some(at("2026-01-02T09:00:00Z"))),
                (4, Some(at("2026-01-02T18:00:00Z"))),
            ]
        );
    }

    #[test]
    fn upcoming_uses_fallbacks_and_puts_unreachable_posts_last() {
        let slots = [make_slot(1, "09:00", vec![pick("gi", 2, &["hsr"])])];
        let candidates = [
            candidate(1, "other"),
            candidate(2, "hsr"),
            candidate(3, "gi"),
            candidate(4, "hsr"),
        ];
        let now = at("2026-01-01T12:00:00Z");
        let upcoming = upcoming(&slots, &TimeZone::UTC, now, &candidates).unwrap();
        assert_eq!(
            order(&upcoming),
            [
                (3, Some(at("2026-01-02T09:00:00Z"))),
                (2, Some(at("2026-01-02T09:00:00Z"))),
                (4, Some(at("2026-01-03T09:00:00Z"))),
                (1, None),
            ]
        );
    }

    #[test]
    fn upcoming_skips_empty_and_not_yet_effective_slots() {
        let mut later = make_slot(1, "10:00", vec![pick("gi", 1, &[])]);
        later.effective_from = at("2026-01-02T00:00:00Z");
        let slots = [later, make_slot(2, "11:00", Vec::new())];
        let candidates = [candidate(1, "gi"), candidate(2, "gi")];
        let now = at("2026-01-01T08:00:00Z");
        let upcoming = upcoming(&slots, &TimeZone::UTC, now, &candidates).unwrap();
        assert_eq!(
            order(&upcoming),
            [
                (1, Some(at("2026-01-02T10:00:00Z"))),
                (2, Some(at("2026-01-03T10:00:00Z"))),
            ]
        );
    }

    #[test]
    fn without_slots_nothing_is_ever_sent() {
        let candidates = [candidate(1, "gi"), candidate(2, "hsr")];
        let upcoming = upcoming(&[], &TimeZone::UTC, Timestamp::UNIX_EPOCH, &candidates).unwrap();
        assert_eq!(order(&upcoming), [(1, None), (2, None)]);
    }
}
