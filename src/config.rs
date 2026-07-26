use std::{collections::BTreeMap, env, fs, net::IpAddr, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "version")]
    pub version: u8,
    #[serde(default)]
    pub listen: ListenConfig,
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub downstream_auth: Option<AuthConfig>,
    #[serde(default)]
    pub refresh: RefreshConfig,
    #[serde(default)]
    pub delete: DeleteConfig,
    #[serde(default)]
    pub state: StateConfig,
    #[serde(default)]
    pub on_library_update: Option<Vec<String>>,
    pub directories: BTreeMap<String, DirectoryConfig>,
}

fn version() -> u8 {
    1
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
    #[serde(default = "default_host")]
    pub host: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
}
fn default_host() -> IpAddr {
    "127.0.0.1".parse().unwrap()
}
fn default_port() -> u16 {
    9999
}
impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub url: Url,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default)]
    pub password_file: Option<String>,
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_header_timeout_secs")]
    pub header_timeout_secs: u64,
    #[serde(default = "default_crawl_concurrency")]
    pub crawl_concurrency: usize,
    #[serde(default)]
    pub allow_http: bool,
}
fn default_connect_timeout_secs() -> u64 {
    10
}
fn default_header_timeout_secs() -> u64 {
    30
}
fn default_crawl_concurrency() -> usize {
    8
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub username: String,
    #[serde(default)]
    pub password_env: Option<String>,
    #[serde(default)]
    pub password_file: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshConfig {
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_full_interval")]
    pub full_interval_secs: u64,
    #[serde(default)]
    pub hook: Option<RefreshHookConfig>,
}
fn default_interval() -> u64 {
    5
}
fn default_full_interval() -> u64 {
    900
}
impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_interval(),
            full_interval_secs: default_full_interval(),
            hook: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshHookConfig {
    pub url: Url,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_interval")]
    pub cooldown_secs: u64,
    #[serde(default)]
    pub reuse_upstream_auth: bool,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}
fn default_method() -> String {
    "GET".into()
}
fn default_hook_timeout() -> u64 {
    15
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConfig {
    #[serde(default = "default_db")]
    pub database: String,
}
fn default_db() -> String {
    "state/index.db".into()
}
impl Default for StateConfig {
    fn default() -> Self {
        Self {
            database: default_db(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryConfig {
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub group_order: i64,
    #[serde(default)]
    pub only_show_the_biggest_file: bool,
    #[serde(default)]
    pub only_show_files_with_size_lte: Option<u64>,
    #[serde(default)]
    pub only_show_files_with_size_gte: Option<u64>,
    pub filters: Vec<FilterConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum FilterConfig {
    Id {
        id: String,
    },
    Regex {
        regex: String,
    },
    NotRegex {
        not_regex: String,
    },
    Contains {
        contains: String,
    },
    ContainsStrict {
        contains_strict: String,
    },
    NotContains {
        not_contains: String,
    },
    NotContainsStrict {
        not_contains_strict: String,
    },
    AnyFileInsideRegex {
        any_file_inside_regex: String,
    },
    AnyFileInsideNotRegex {
        any_file_inside_not_regex: String,
    },
    AnyFileInsideContains {
        any_file_inside_contains: String,
    },
    AnyFileInsideContainsStrict {
        any_file_inside_contains_strict: String,
    },
    AnyFileInsideNotContains {
        any_file_inside_not_contains: String,
    },
    AnyFileInsideNotContainsStrict {
        any_file_inside_not_contains_strict: String,
    },
    HasEpisodes {
        has_episodes: bool,
    },
    SizeGte {
        size_gte: u64,
    },
    SizeLte {
        size_lte: u64,
    },
    AnyFileInsideSizeGte {
        any_file_inside_size_gte: u64,
    },
    AnyFileInsideSizeLte {
        any_file_inside_size_lte: u64,
    },
    And {
        and: Vec<FilterConfig>,
    },
    Or {
        or: Vec<FilterConfig>,
    },
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let config: Self =
            serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported config version {}", self.version);
        }
        if self.directories.is_empty() {
            bail!("at least one directory is required");
        }
        if self.refresh.interval_secs == 0 {
            bail!("refresh.interval_secs must be greater than zero");
        }
        if self.refresh.interval_secs < 5 {
            bail!("refresh.interval_secs must be at least 5 seconds");
        }
        if let Some(hook) = &self.refresh.hook
            && hook.cooldown_secs < 5
        {
            bail!("refresh.hook.cooldown_secs must be at least 5 seconds");
        }
        if self.upstream.crawl_concurrency == 0 {
            bail!("upstream.crawl_concurrency must be greater than zero");
        }
        if self.upstream.url.scheme() != "https" && !self.upstream.allow_http {
            bail!("upstream URL must use HTTPS unless upstream.allow_http is true");
        }
        validate_secret(
            "upstream",
            &self.upstream.password_env,
            &self.upstream.password_file,
        )?;
        if let Some(auth) = &self.downstream_auth {
            validate_secret("downstream_auth", &auth.password_env, &auth.password_file)?;
        }
        Ok(())
    }

    pub fn upstream_password(&self) -> Result<Option<String>> {
        read_secret(&self.upstream.password_env, &self.upstream.password_file)
    }

    pub fn downstream_password(&self) -> Result<Option<String>> {
        match &self.downstream_auth {
            Some(auth) => read_secret(&auth.password_env, &auth.password_file),
            None => Ok(None),
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.refresh.interval_secs)
    }
}

fn validate_secret(label: &str, env_name: &Option<String>, file: &Option<String>) -> Result<()> {
    if env_name.is_some() && file.is_some() {
        bail!("{label}: choose password_env or password_file, not both");
    }
    Ok(())
}

fn read_secret(env_name: &Option<String>, file: &Option<String>) -> Result<Option<String>> {
    if let Some(name) = env_name {
        return Ok(Some(env::var(name).with_context(|| {
            format!("environment variable {name} is not set")
        })?));
    }
    if let Some(path) = file {
        return Ok(Some(
            fs::read_to_string(path)
                .with_context(|| format!("read secret file {path}"))?
                .trim_end()
                .to_owned(),
        ));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plain_http_by_default() {
        let yaml = r#"
version: 1
upstream:
  url: http://example.test/dav/
directories:
  movies:
    filters:
      - regex: /.*/
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn fast_refresh_defaults_and_floor_are_enforced() {
        let yaml = r#"
version: 1
upstream:
  url: https://example.test/dav/
directories:
  movies:
    filters:
      - regex: /.*/
"#;
        let defaulted: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(defaulted.refresh.interval_secs, 5);

        let too_fast = yaml.replace("version: 1", "version: 1\nrefresh:\n  interval_secs: 4");
        let config: Config = serde_yaml::from_str(&too_fast).unwrap();
        assert!(config.validate().is_err());
    }
}
