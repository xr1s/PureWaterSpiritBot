//! 领域模型与持久化层边界上的受检转换。

use anyhow::{Context, Result};
use jiff::Timestamp;
use teloxide::types::UserId;

pub(super) fn timestamp(value: Timestamp) -> Result<i64> {
    Ok(value.as_microsecond())
}

pub(super) fn decode_timestamp(value: i64) -> Result<Timestamp> {
    Timestamp::from_microsecond(value).context("invalid stored timestamp")
}

pub(super) fn user_id(value: UserId) -> Result<i64> {
    i64::try_from(value.0).context("Telegram user ID exceeds signed BIGINT")
}

pub(super) fn decode_user_id(value: i64) -> Result<UserId> {
    Ok(UserId(
        u64::try_from(value).context("negative stored Telegram user ID")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_ids_are_checked_in_both_directions() {
        assert_eq!(user_id(UserId(i64::MAX as u64)).unwrap(), i64::MAX);
        assert!(user_id(UserId(i64::MAX as u64 + 1)).is_err());
        assert!(decode_user_id(-1).is_err());
        assert_eq!(decode_user_id(0).unwrap(), UserId(0));
    }

    #[test]
    fn timestamp_precision_is_microseconds() {
        let value = Timestamp::from_microsecond(1_234_567).unwrap();
        assert_eq!(timestamp(value).unwrap(), 1_234_567);
        assert_eq!(decode_timestamp(1_234_567).unwrap(), value);
        assert!(decode_timestamp(i64::MAX).is_err());
    }
}
