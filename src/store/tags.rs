// 自动标签所用的词表。

use std::collections::HashMap;

use anyhow::Result;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::{
    Store,
    entities::{tag_groups, tags},
};

/// 一个标签分组和其中未归档的标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagGroup {
    pub label: String,
    pub tags: Vec<String>,
}

impl Store {
    /// 所有未归档的标签，按分组整理：分组按显示顺序，分组内的标签按添加顺序。
    /// 没有标签的分组不返回。
    pub async fn tag_groups(&self) -> Result<Vec<TagGroup>> {
        let groups = tag_groups::Entity::find()
            .filter(tag_groups::Column::ArchivedAt.is_null())
            .order_by_asc(tag_groups::Column::Position)
            .order_by_asc(tag_groups::Column::Id)
            .all(&self.db)
            .await?;
        let rows = tags::Entity::find()
            .filter(tags::Column::ArchivedAt.is_null())
            .filter(tags::Column::GroupId.is_in(groups.iter().map(|group| group.id)))
            .order_by_asc(tags::Column::Id)
            .all(&self.db)
            .await?;
        let mut names: HashMap<i64, Vec<String>> = HashMap::new();
        for row in rows {
            names.entry(row.group_id).or_default().push(row.name);
        }
        Ok(groups
            .into_iter()
            .filter_map(|group| {
                names.remove(&group.id).map(|tags| TagGroup {
                    label: group.label,
                    tags,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, Set};

    use super::*;
    use crate::store::Store;
    use crate::store::test_support::insert_tag;

    #[tokio::test]
    async fn groups_the_tags_and_skips_archived_ones() {
        let (_directory, store) = Store::open_temporary().await;
        assert!(store.tag_groups().await.unwrap().is_empty());

        insert_tag(&store, "原神", "钟离").await;
        insert_tag(&store, "崩铁", "流萤").await;
        insert_tag(&store, "原神", "甘雨").await;
        insert_tag(&store, "原神", "旧标签").await;
        insert_tag(&store, "空分组", "将被归档").await;
        {
            tags::Entity::update_many()
                .filter(tags::Column::Name.eq("旧标签"))
                .set(tags::ActiveModel {
                    archived_at: Set(Some(1)),
                    active_name: Set(None),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
            tag_groups::Entity::update_many()
                .filter(tag_groups::Column::Label.eq("空分组"))
                .set(tag_groups::ActiveModel {
                    archived_at: Set(Some(1)),
                    active_label: Set(None),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
        }

        let groups = store.tag_groups().await.unwrap();
        assert_eq!(
            groups,
            [
                TagGroup {
                    label: "原神".to_owned(),
                    tags: vec!["钟离".to_owned(), "甘雨".to_owned()],
                },
                TagGroup {
                    label: "崩铁".to_owned(),
                    tags: vec!["流萤".to_owned()],
                },
            ]
        );
    }

    #[tokio::test]
    async fn tag_names_are_unique_ignoring_case() {
        let (_directory, store) = Store::open_temporary().await;
        insert_tag(&store, "a", "Hu Tao").await;
        let group = tag_groups::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let duplicate = tags::ActiveModel {
            group_id: Set(group.id),
            name: Set("hu tao".into()),
            active_name: Set(Some("hu tao".into())),
            archived_at: Set(None),
            ..Default::default()
        }
        .insert(&store.db)
        .await;
        assert!(duplicate.is_err());
    }
}
