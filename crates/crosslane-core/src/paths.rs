//! 配置与数据文件路径解析。

use std::path::PathBuf;

/// 配置与数据文件的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// 配置目录。
    pub config_dir: PathBuf,
    /// 数据目录。
    pub data_dir: PathBuf,
    /// 配置文件。
    pub config_file: PathBuf,
    /// 账号文件。
    pub accounts_file: PathBuf,
    /// 当日额度文件。
    pub daily_file: PathBuf,
    /// 累计统计文件。
    pub stats_file: PathBuf,
    /// 持久化的告警事件去重状态。
    pub alert_incidents_file: PathBuf,
    /// Web Search opaque replay records的本地加密密钥。
    pub web_search_replay_key_file: PathBuf,
}

impl Paths {
    /// 从进程环境解析路径。
    pub fn from_env() -> Self {
        Self::from_env_values(
            std::env::var("CROSSLANE_HOME").ok().as_deref(),
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("XDG_DATA_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    /// 纯函数版本，便于测试。
    pub fn from_env_values(
        crosslane_home: Option<&str>,
        xdg_config: Option<&str>,
        xdg_data: Option<&str>,
        home: Option<&str>,
    ) -> Self {
        let (config_dir, data_dir) = match crosslane_home {
            Some(root) => (PathBuf::from(root), PathBuf::from(root)),
            None => {
                let config = xdg_config.map_or_else(
                    || {
                        home.map_or_else(
                            || PathBuf::from(".crosslane"),
                            |value| PathBuf::from(value).join(".config").join("crosslane"),
                        )
                    },
                    |value| PathBuf::from(value).join("crosslane"),
                );
                let data = xdg_data.map_or_else(
                    || {
                        home.map_or_else(
                            || PathBuf::from(".crosslane"),
                            |value| {
                                PathBuf::from(value)
                                    .join(".local")
                                    .join("share")
                                    .join("crosslane")
                            },
                        )
                    },
                    |value| PathBuf::from(value).join("crosslane"),
                );
                (config, data)
            }
        };

        Self {
            config_file: config_dir.join("config.toml"),
            accounts_file: data_dir.join("accounts.json"),
            daily_file: data_dir.join("daily.json"),
            stats_file: data_dir.join("stats.json"),
            alert_incidents_file: data_dir.join("alert-incidents.json"),
            web_search_replay_key_file: data_dir.join("web-search-replay.key"),
            config_dir,
            data_dir,
        }
    }
}

/// 便捷入口，等价于 [`Paths::from_env`]。
pub fn resolve_paths() -> Paths {
    Paths::from_env()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crosslane_home_overrides_everything() {
        let paths = Paths::from_env_values(Some("/custom/crosslane"), None, None, Some("/home/u"));
        assert_eq!(paths.config_dir, PathBuf::from("/custom/crosslane"));
        assert_eq!(paths.data_dir, PathBuf::from("/custom/crosslane"));
        assert_eq!(
            paths.config_file,
            PathBuf::from("/custom/crosslane/config.toml")
        );
        assert_eq!(
            paths.accounts_file,
            PathBuf::from("/custom/crosslane/accounts.json")
        );
    }

    #[test]
    fn xdg_variables_are_used_when_crosslane_home_absent() {
        let paths =
            Paths::from_env_values(None, Some("/x/config"), Some("/x/data"), Some("/home/u"));
        assert_eq!(paths.config_dir, PathBuf::from("/x/config/crosslane"));
        assert_eq!(paths.data_dir, PathBuf::from("/x/data/crosslane"));
        assert_eq!(
            paths.daily_file,
            PathBuf::from("/x/data/crosslane/daily.json")
        );
        assert_eq!(
            paths.stats_file,
            PathBuf::from("/x/data/crosslane/stats.json")
        );
        assert_eq!(
            paths.web_search_replay_key_file,
            PathBuf::from("/x/data/crosslane/web-search-replay.key")
        );
    }

    #[test]
    fn falls_back_to_home_relative_defaults() {
        let paths = Paths::from_env_values(None, None, None, Some("/home/u"));
        assert_eq!(paths.config_dir, PathBuf::from("/home/u/.config/crosslane"));
        assert_eq!(
            paths.data_dir,
            PathBuf::from("/home/u/.local/share/crosslane")
        );
    }

    #[test]
    fn falls_back_to_current_dir_when_home_unknown() {
        let paths = Paths::from_env_values(None, None, None, None);
        assert_eq!(paths.config_dir, PathBuf::from(".crosslane"));
        assert_eq!(paths.data_dir, PathBuf::from(".crosslane"));
    }
}
