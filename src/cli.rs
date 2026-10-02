use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about = "Telegram content publication bot")]
pub struct Args {
    /// 要加载的配置文件。
    #[arg(long, value_name = "FILE", default_value = "PureWaterSpiritBot.toml")]
    pub config: PathBuf,

    /// 初始化数据库 schema，不加载配置，也不连接 Telegram。
    #[arg(long, value_name = "DATABASE_URL")]
    pub init_db: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn default_config_is_in_the_working_directory() {
        let args = Args::try_parse_from(["bot"]).unwrap();
        assert_eq!(args.config, PathBuf::from("PureWaterSpiritBot.toml"));
        assert!(args.init_db.is_none());
    }

    #[test]
    fn config_selects_a_file_including_spaces() {
        let args =
            Args::try_parse_from(["bot", "--config", "/some directory/custom.toml"]).unwrap();
        assert_eq!(args.config, PathBuf::from("/some directory/custom.toml"));
    }

    #[test]
    fn init_db_remains_available_with_config() {
        let args = Args::try_parse_from([
            "bot",
            "--init-db",
            "sqlite://new.db?mode=rwc",
            "--config",
            "settings/custom.toml",
        ])
        .unwrap();
        assert_eq!(args.init_db.as_deref(), Some("sqlite://new.db?mode=rwc"));
        assert_eq!(args.config, PathBuf::from("settings/custom.toml"));
    }

    #[test]
    fn invalid_arguments_are_rejected_by_clap() {
        for args in [
            vec!["bot", "--config"],
            vec!["bot", "--init-db"],
            vec!["bot", "--unknown"],
            vec!["bot", "unexpected"],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn help_and_version_are_handled_by_clap() {
        assert_eq!(
            Args::try_parse_from(["bot", "--help"]).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            Args::try_parse_from(["bot", "--version"])
                .unwrap_err()
                .kind(),
            ErrorKind::DisplayVersion
        );
    }

    #[test]
    fn selected_configuration_can_be_loaded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom.toml");
        let args = Args::try_parse_from([
            std::ffi::OsString::from("bot"),
            std::ffi::OsString::from("--config"),
            path.as_os_str().to_owned(),
        ])
        .unwrap();
        std::fs::write(
            &args.config,
            include_str!("../PureWaterSpiritBot.example.toml"),
        )
        .unwrap();
        assert!(crate::config::Config::load(&args.config).is_ok());
    }
}
