//! 编辑分类、时间表和设置用的面板：正文加内联键盘。
//!
//! 这里只做渲染，不访问数据库和 Telegram，输入是已经加载好的 [`AdminSchedule`]。

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use super::callback::{CallbackData, EditAction, SettingKind};
use crate::{
    model::{Category, CategoryId, Pick, PickId, Slot, SlotId},
    schedule::{AdminSchedule, format_clock, format_duration},
};

pub struct Panel {
    pub text: String,
    pub keyboard: InlineKeyboardMarkup,
}

fn button(text: impl Into<String>, action: EditAction) -> InlineKeyboardButton {
    InlineKeyboardButton::callback(text, CallbackData::Edit(action).encode())
}

fn back(action: EditAction) -> Vec<InlineKeyboardButton> {
    vec![button("« 返回", action)]
}

fn label_of(schedule: &AdminSchedule, id: CategoryId) -> &str {
    category_of(schedule, id).map_or("（已删除的分类）", |category| {
        category.label.as_str()
    })
}

fn category_of(schedule: &AdminSchedule, id: CategoryId) -> Option<&Category> {
    schedule
        .categories
        .iter()
        .find(|category| category.id == id)
}

fn slot_of(schedule: &AdminSchedule, id: SlotId) -> Option<&Slot> {
    schedule.slots.iter().find(|slot| slot.id == id)
}

/// 配额所在的时段和配额本身。
pub fn find_pick(schedule: &AdminSchedule, id: PickId) -> Option<(&Slot, &Pick)> {
    schedule.slots.iter().find_map(|slot| {
        slot.picks
            .iter()
            .find(|pick| pick.id == id)
            .map(|pick| (slot, pick))
    })
}

/// 一项配额的简述，例如 `原神 ×2（不足时补：其他 → 崩铁）`。
fn describe_pick(schedule: &AdminSchedule, pick: &Pick) -> String {
    let mut text = format!("{} ×{}", label_of(schedule, pick.category), pick.count);
    if !pick.fallback.is_empty() {
        let names: Vec<_> = pick
            .fallback
            .iter()
            .map(|id| label_of(schedule, *id))
            .collect();
        text.push_str(&format!("（不足时补：{}）", names.join(" → ")));
    }
    text
}

/// 面板找不到对象时显示的内容，比如对象刚被删除。
pub fn missing(back_to: EditAction) -> Panel {
    Panel {
        text: "找不到这一项，它可能已经被删除。".to_owned(),
        keyboard: InlineKeyboardMarkup::new([back(back_to)]),
    }
}

pub fn categories(schedule: &AdminSchedule) -> Panel {
    let text = if schedule.categories.is_empty() {
        "你还没有分类。点「新增分类」创建第一个。\n\n\
         分类是投稿的队列，发布时段会按配额从中取稿。"
            .to_owned()
    } else {
        let lines: Vec<_> = schedule
            .categories
            .iter()
            .enumerate()
            .map(|(index, category)| format!("{}. {}", index + 1, category.label))
            .collect();
        format!(
            "你的分类：\n{}\n\n点击分类可以改名、调整顺序或删除。",
            lines.join("\n")
        )
    };
    let mut rows: Vec<_> = schedule
        .categories
        .iter()
        .map(|category| vec![button(&category.label, EditAction::Category(category.id))])
        .collect();
    rows.push(vec![button("➕ 新增分类", EditAction::NewCategory)]);
    Panel {
        text,
        keyboard: InlineKeyboardMarkup::new(rows),
    }
}

pub fn category(schedule: &AdminSchedule, id: CategoryId) -> Panel {
    let Some(category) = category_of(schedule, id) else {
        return missing(EditAction::Categories);
    };
    Panel {
        text: format!("分类「{}」", category.label),
        keyboard: InlineKeyboardMarkup::new([
            vec![button("改名", EditAction::RenameCategory(id))],
            vec![
                button(
                    "↑ 上移",
                    EditAction::MoveCategory {
                        category: id,
                        up: true,
                    },
                ),
                button(
                    "↓ 下移",
                    EditAction::MoveCategory {
                        category: id,
                        up: false,
                    },
                ),
            ],
            vec![button(
                "删除分类",
                EditAction::ArchiveCategory {
                    category: id,
                    confirmed: false,
                },
            )],
            back(EditAction::Categories),
        ]),
    }
}

pub fn confirm_archive_category(schedule: &AdminSchedule, id: CategoryId) -> Panel {
    let Some(category) = category_of(schedule, id) else {
        return missing(EditAction::Categories);
    };
    Panel {
        text: format!(
            "确定删除分类「{}」吗？\n已发布的投稿不受影响。",
            category.label
        ),
        keyboard: InlineKeyboardMarkup::new([vec![
            button(
                "确认删除",
                EditAction::ArchiveCategory {
                    category: id,
                    confirmed: true,
                },
            ),
            button("取消", EditAction::Category(id)),
        ]]),
    }
}

pub fn schedule(schedule: &AdminSchedule) -> Panel {
    let text = if schedule.slots.is_empty() {
        format!(
            "你还没有发布时段。点「新增时段」创建。\n\n时间按时区 {} 计算。",
            zone_name(schedule)
        )
    } else {
        let lines: Vec<_> = schedule
            .slots
            .iter()
            .map(|slot| {
                format!(
                    "{}：{}",
                    format_clock(slot.time),
                    summarize_slot(schedule, slot)
                )
            })
            .collect();
        format!(
            "你的发布时段（时区 {}）：\n{}",
            zone_name(schedule),
            lines.join("\n")
        )
    };
    let mut rows: Vec<_> = schedule
        .slots
        .iter()
        .map(|slot| vec![button(format_clock(slot.time), EditAction::Slot(slot.id))])
        .collect();
    rows.push(vec![button("➕ 新增时段", EditAction::NewSlot)]);
    Panel {
        text,
        keyboard: InlineKeyboardMarkup::new(rows),
    }
}

fn zone_name(schedule: &AdminSchedule) -> &str {
    schedule.timezone.iana_name().unwrap_or("自定义偏移")
}

fn summarize_slot(schedule: &AdminSchedule, slot: &Slot) -> String {
    if slot.picks.is_empty() {
        return "还没有配额".to_owned();
    }
    let parts: Vec<_> = slot
        .picks
        .iter()
        .map(|pick| format!("{}×{}", label_of(schedule, pick.category), pick.count))
        .collect();
    parts.join(" ")
}

pub fn slot(schedule: &AdminSchedule, id: SlotId) -> Panel {
    let Some(slot) = slot_of(schedule, id) else {
        return missing(EditAction::Schedule);
    };
    let mut text = format!(
        "{} 时段（时区 {}）\n",
        format_clock(slot.time),
        zone_name(schedule)
    );
    if slot.picks.is_empty() {
        text.push_str("\n还没有配额。点「添加配额」决定每天在这个时间发布哪些分类、各几条。");
    } else {
        text.push_str("\n每天从这些分类取稿：\n");
        for pick in &slot.picks {
            text.push_str(&format!("· {}\n", describe_pick(schedule, pick)));
        }
        text.push_str("\n点击配额可以修改数量和补位。");
    }
    let mut rows: Vec<_> = slot
        .picks
        .iter()
        .map(|pick| {
            vec![button(
                format!("{} ×{}", label_of(schedule, pick.category), pick.count),
                EditAction::Pick(pick.id),
            )]
        })
        .collect();
    rows.push(vec![button("➕ 添加配额", EditAction::AddPick(id))]);
    rows.push(vec![
        button("改时间", EditAction::SlotTime(id)),
        button(
            "删除时段",
            EditAction::DeleteSlot {
                slot: id,
                confirmed: false,
            },
        ),
    ]);
    rows.push(back(EditAction::Schedule));
    Panel {
        text,
        keyboard: InlineKeyboardMarkup::new(rows),
    }
}

pub fn confirm_delete_slot(schedule: &AdminSchedule, id: SlotId) -> Panel {
    let Some(slot) = slot_of(schedule, id) else {
        return missing(EditAction::Schedule);
    };
    Panel {
        text: format!(
            "确定删除 {} 时段吗？它的配额也会一起删除。",
            format_clock(slot.time)
        ),
        keyboard: InlineKeyboardMarkup::new([vec![
            button(
                "确认删除",
                EditAction::DeleteSlot {
                    slot: id,
                    confirmed: true,
                },
            ),
            button("取消", EditAction::Slot(id)),
        ]]),
    }
}

/// 选择分类的面板：列出 `exclude` 之外的分类，点击后执行 `action(分类)`。
fn choose_category(
    schedule: &AdminSchedule,
    title: &str,
    exclude: &[CategoryId],
    action: impl Fn(CategoryId) -> EditAction,
    back_to: EditAction,
) -> Panel {
    let choices: Vec<_> = schedule
        .categories
        .iter()
        .filter(|category| !exclude.contains(&category.id))
        .collect();
    let text = if choices.is_empty() {
        format!("{title}\n\n没有可选的分类了。")
    } else {
        title.to_owned()
    };
    let mut rows: Vec<_> = choices
        .chunks(2)
        .map(|chunk| {
            chunk
                .iter()
                .map(|category| button(&category.label, action(category.id)))
                .collect()
        })
        .collect();
    rows.push(back(back_to));
    Panel {
        text,
        keyboard: InlineKeyboardMarkup::new(rows),
    }
}

pub fn choose_pick_category(schedule: &AdminSchedule, id: SlotId) -> Panel {
    let Some(slot) = slot_of(schedule, id) else {
        return missing(EditAction::Schedule);
    };
    let taken: Vec<_> = slot.picks.iter().map(|pick| pick.category).collect();
    choose_category(
        schedule,
        &format!("给 {} 时段添加配额：选择分类", format_clock(slot.time)),
        &taken,
        |category| EditAction::AddPickCategory { slot: id, category },
        EditAction::Slot(id),
    )
}

pub fn pick(schedule: &AdminSchedule, id: PickId) -> Panel {
    let Some((slot, pick)) = find_pick(schedule, id) else {
        return missing(EditAction::Schedule);
    };
    let mut text = format!(
        "{} 时段：{}\n\n数量：{}\n",
        format_clock(slot.time),
        label_of(schedule, pick.category),
        pick.count
    );
    if pick.fallback.is_empty() {
        text.push_str("补位：无。这个分类的稿子不够时，就少发。");
    } else {
        text.push_str("这个分类的稿子不够时，按顺序从这些分类补位：\n");
        for (index, id) in pick.fallback.iter().enumerate() {
            text.push_str(&format!("{}. {}\n", index + 1, label_of(schedule, *id)));
        }
        text.push_str("\n点击补位分类可以把它去掉。");
    }
    let mut rows: Vec<_> = pick
        .fallback
        .iter()
        .filter_map(|fallback| {
            let category = category_of(schedule, *fallback)?;
            Some(vec![button(
                format!("✖ {}", category.label),
                EditAction::RemoveFallback {
                    pick: id,
                    category: category.id,
                },
            )])
        })
        .collect();
    rows.push(vec![button("➕ 添加补位", EditAction::AddFallback(id))]);
    rows.push(vec![
        button("改数量", EditAction::PickCount(id)),
        button("删除配额", EditAction::DeletePick(id)),
    ]);
    rows.push(back(EditAction::Slot(slot.id)));
    Panel {
        text,
        keyboard: InlineKeyboardMarkup::new(rows),
    }
}

pub fn choose_fallback_category(schedule: &AdminSchedule, id: PickId) -> Panel {
    let Some((slot, pick)) = find_pick(schedule, id) else {
        return missing(EditAction::Schedule);
    };
    let mut excluded = pick.fallback.clone();
    excluded.push(pick.category);
    choose_category(
        schedule,
        &format!(
            "给 {} 的{}配额添加补位：选择分类（排在已有补位之后）",
            format_clock(slot.time),
            label_of(schedule, pick.category)
        ),
        &excluded,
        |category| EditAction::AddFallbackCategory { pick: id, category },
        EditAction::Pick(id),
    )
}

pub fn settings(schedule: &AdminSchedule) -> Panel {
    let reminder = schedule
        .reminder
        .map_or_else(|| "关闭".to_owned(), format_clock);
    let text = format!(
        "你的设置：\n\
         · 时区：{}\n\
         · 补发宽限期：{}（错过时段后，在这段时间内仍会补发）\n\
         · 发送间隔：{}（同一时段内两条投稿之间）\n\
         · 库存提醒：{}（每天这个时间私聊你当前库存）",
        zone_name(schedule),
        format_duration(schedule.misfire_grace),
        format_duration(schedule.send_interval),
        reminder,
    );
    Panel {
        text,
        keyboard: InlineKeyboardMarkup::new([
            vec![button("时区", EditAction::Setting(SettingKind::Timezone))],
            vec![button(
                "补发宽限期",
                EditAction::Setting(SettingKind::MisfireGrace),
            )],
            vec![button(
                "发送间隔",
                EditAction::Setting(SettingKind::SendInterval),
            )],
            vec![button(
                "库存提醒",
                EditAction::Setting(SettingKind::Reminder),
            )],
        ]),
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;
    use crate::{
        model::PositiveDuration,
        schedule::tests::{category as make_category, pick as make_pick, slot as make_slot},
    };

    fn sample() -> AdminSchedule {
        let mut first = make_pick("gi", 2, &["other", "hsr"]);
        first.id = PickId(10);
        let mut second = make_pick("hsr", 1, &[]);
        second.id = PickId(11);
        AdminSchedule {
            admin: teloxide::types::UserId(1),
            timezone: TimeZone::get("Asia/Shanghai").unwrap(),
            misfire_grace: PositiveDuration::try_from(jiff::SignedDuration::from_hours(2)).unwrap(),
            send_interval: PositiveDuration::try_from(jiff::SignedDuration::from_secs(3)).unwrap(),
            reminder: Some("20:00".parse().unwrap()),
            categories: vec![
                make_category(1, "原神"),
                make_category(2, "崩铁"),
                make_category(3, "其他"),
            ],
            slots: vec![
                make_slot(5, "10:00", vec![first, second]),
                make_slot(6, "14:00", Vec::new()),
            ],
        }
    }

    fn callbacks(panel: &Panel) -> Vec<EditAction> {
        panel
            .keyboard
            .inline_keyboard
            .iter()
            .flatten()
            .filter_map(|button| match &button.kind {
                teloxide::types::InlineKeyboardButtonKind::CallbackData(data) => {
                    match CallbackData::decode(data) {
                        Some(CallbackData::Edit(action)) => Some(action),
                        _ => panic!("not an edit action: {data}"),
                    }
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lists_categories_and_offers_to_add_one() {
        let panel = categories(&sample());
        assert!(panel.text.contains("1. 原神"));
        assert!(panel.text.contains("3. 其他"));
        let actions = callbacks(&panel);
        assert!(actions.contains(&EditAction::Category(CategoryId(2))));
        assert_eq!(actions.last(), Some(&EditAction::NewCategory));
    }

    #[test]
    fn explains_the_empty_state() {
        let mut empty = sample();
        empty.categories.clear();
        empty.slots.clear();
        assert!(categories(&empty).text.contains("还没有分类"));
        assert!(schedule(&empty).text.contains("还没有发布时段"));
    }

    #[test]
    fn describes_slots_with_picks_and_fallbacks() {
        let sample = sample();
        let list = schedule(&sample);
        assert!(list.text.contains("10:00：原神×2 崩铁×1"));
        assert!(list.text.contains("14:00：还没有配额"));
        assert!(list.text.contains("Asia/Shanghai"));

        let detail = slot(&sample, SlotId(5));
        assert!(detail.text.contains("原神 ×2（不足时补：其他 → 崩铁）"));
        let actions = callbacks(&detail);
        assert!(actions.contains(&EditAction::Pick(PickId(10))));
        assert!(actions.contains(&EditAction::AddPick(SlotId(5))));
        assert!(actions.contains(&EditAction::SlotTime(SlotId(5))));
    }

    #[test]
    fn category_choosers_leave_out_used_categories() {
        let sample = sample();
        // 10:00 时段已经有 gi 和 hsr 的配额，只剩 other。
        let panel = choose_pick_category(&sample, SlotId(5));
        assert_eq!(
            callbacks(&panel)
                .into_iter()
                .filter(|action| matches!(action, EditAction::AddPickCategory { .. }))
                .collect::<Vec<_>>(),
            [EditAction::AddPickCategory {
                slot: SlotId(5),
                category: CategoryId(3)
            }]
        );
        // gi 的补位已经有 other 和 hsr，自己也不能当补位，没有可选项。
        let panel = choose_fallback_category(&sample, PickId(10));
        assert!(panel.text.contains("没有可选的分类了"));
        // hsr 的配额还可以补 gi 和 other。
        let panel = choose_fallback_category(&sample, PickId(11));
        assert_eq!(
            callbacks(&panel)
                .into_iter()
                .filter(|action| matches!(action, EditAction::AddFallbackCategory { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn fallbacks_can_be_removed_from_the_pick_panel() {
        let panel = pick(&sample(), PickId(10));
        let actions = callbacks(&panel);
        assert!(actions.contains(&EditAction::RemoveFallback {
            pick: PickId(10),
            category: CategoryId(3)
        }));
        assert!(actions.contains(&EditAction::DeletePick(PickId(10))));
        assert_eq!(actions.last(), Some(&EditAction::Slot(SlotId(5))));
    }

    #[test]
    fn destructive_actions_ask_first() {
        let sample = sample();
        let panel = category(&sample, CategoryId(1));
        assert!(callbacks(&panel).contains(&EditAction::ArchiveCategory {
            category: CategoryId(1),
            confirmed: false
        }));
        assert!(
            callbacks(&confirm_archive_category(&sample, CategoryId(1))).contains(
                &EditAction::ArchiveCategory {
                    category: CategoryId(1),
                    confirmed: true
                }
            )
        );
        assert!(
            callbacks(&slot(&sample, SlotId(5))).contains(&EditAction::DeleteSlot {
                slot: SlotId(5),
                confirmed: false
            })
        );
        assert!(
            callbacks(&confirm_delete_slot(&sample, SlotId(5))).contains(&EditAction::DeleteSlot {
                slot: SlotId(5),
                confirmed: true
            })
        );
    }

    #[test]
    fn unknown_objects_fall_back_to_the_parent_panel() {
        let sample = sample();
        assert_eq!(
            callbacks(&category(&sample, CategoryId(99))),
            [EditAction::Categories]
        );
        assert_eq!(
            callbacks(&slot(&sample, SlotId(99))),
            [EditAction::Schedule]
        );
        assert_eq!(
            callbacks(&pick(&sample, PickId(99))),
            [EditAction::Schedule]
        );
    }

    #[test]
    fn shows_the_settings() {
        let panel = settings(&sample());
        assert!(panel.text.contains("Asia/Shanghai"));
        assert!(panel.text.contains("2h"));
        assert!(panel.text.contains("3s"));
        assert!(panel.text.contains("20:00"));
        let mut off = sample();
        off.reminder = None;
        assert!(settings(&off).text.contains("库存提醒：关闭"));
        assert_eq!(callbacks(&panel).len(), 4);
    }
}
