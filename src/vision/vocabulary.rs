use std::collections::HashMap;

/// 词表里的一个分组，例如某个游戏或某个联动系列。分组只用来给模型提供上下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VocabularyGroup {
    pub label: String,
    pub tags: Vec<String>,
}

/// 模型只能从中选择的标签集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vocabulary {
    groups: Vec<VocabularyGroup>,
}

impl Vocabulary {
    /// 没有标签的分组会被丢弃。
    pub fn new(groups: Vec<VocabularyGroup>) -> Self {
        let groups = groups
            .into_iter()
            .filter(|group| !group.tags.is_empty())
            .collect();
        Self { groups }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// 写进提示词的列表：每个分组一行，形如 `[原神] 钟离, 甘雨`。
    /// 内容只取决于词表本身，这样支持前缀缓存的服务可以复用它。
    pub(super) fn listing(&self) -> String {
        self.groups
            .iter()
            .map(|group| format!("[{}] {}", group.label, group.tags.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 把模型给出的名字对应回词表里的标签。不在词表里的名字被丢弃，
    /// 结果去重，并按词表里的顺序排列。
    pub(super) fn resolve<'a>(&self, answers: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut by_key: HashMap<String, usize> = HashMap::new();
        let mut names = Vec::new();
        for tag in self.groups.iter().flat_map(|group| &group.tags) {
            by_key.entry(lookup_key(tag)).or_insert_with(|| {
                names.push(tag.as_str());
                names.len() - 1
            });
        }
        let mut picked: Vec<usize> = answers
            .into_iter()
            .filter_map(|answer| by_key.get(&lookup_key(answer)).copied())
            .collect();
        picked.sort_unstable();
        picked.dedup();
        picked.into_iter().map(|i| names[i].to_owned()).collect()
    }
}

/// 标签名对应的 hashtag：空格换成下划线，前面加 `#`。
/// 名字里有 Telegram 的 hashtag 不支持的字符（标点、`·` 之类）时返回 `None`，
/// 因为这样的 hashtag 会在发布后被截断。
pub fn hashtag(name: &str) -> Option<String> {
    let words: Vec<&str> = name.split_whitespace().collect();
    let valid = !words.is_empty()
        && words
            .iter()
            .all(|word| word.chars().all(|c| c.is_alphanumeric() || c == '_'));
    valid.then(|| format!("#{}", words.join("_")))
}

/// 比较标签名用的键：忽略大小写、开头的 `#`，并把下划线和连续空白视为同一个空格，
/// 因为标签名里的空格在 hashtag 里会变成下划线。
fn lookup_key(name: &str) -> String {
    name.trim()
        .trim_start_matches('#')
        .split(|c: char| c.is_whitespace() || c == '_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
pub(super) mod fixtures {
    use super::*;

    pub fn group(label: &str, tags: &[&str]) -> VocabularyGroup {
        VocabularyGroup {
            label: label.to_owned(),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::group, *};

    fn sample() -> Vocabulary {
        Vocabulary::new(vec![
            group("原神", &["钟离", "Hu Tao", "甘雨"]),
            group("空", &[]),
            group("崩铁", &["流萤"]),
        ])
    }

    #[test]
    fn lists_one_group_per_line_and_drops_empty_groups() {
        assert_eq!(sample().listing(), "[原神] 钟离, Hu Tao, 甘雨\n[崩铁] 流萤");
        assert!(Vocabulary::new(vec![group("空", &[])]).is_empty());
    }

    #[test]
    fn resolves_answers_in_vocabulary_order_without_duplicates() {
        let resolved = sample().resolve(["流萤", "甘雨", "钟离", "甘雨", "不存在"]);
        assert_eq!(resolved, ["钟离", "甘雨", "流萤"]);
    }

    #[test]
    fn matches_hashtag_spelling_and_case() {
        let resolved = sample().resolve(["#hu_tao", "  HU  TAO "]);
        assert_eq!(resolved, ["Hu Tao"]);
    }

    #[test]
    fn builds_hashtags_from_names() {
        assert_eq!(hashtag("钟离").as_deref(), Some("#钟离"));
        assert_eq!(hashtag(" Hu  Tao ").as_deref(), Some("#Hu_Tao"));
        assert_eq!(hashtag("a_b").as_deref(), Some("#a_b"));
        for invalid in ["", "  ", "艾莉丝·某", "a-b", "#a", "a,b"] {
            assert_eq!(hashtag(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn resolves_nothing_for_empty_answers() {
        assert!(sample().resolve([]).is_empty());
    }
}
