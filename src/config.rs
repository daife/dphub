use std::{fs, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub deepseek: DeepSeekConfig,
    pub quota: QuotaConfig,
    pub database: DatabaseConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    pub bind: SocketAddr,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DeepSeekConfig {
    pub api_key: String,
    pub endpoint: String,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct QuotaConfig {
    pub id_daily_limit: i64,
    pub verified_daily_limit: i64,
    pub referral_new_user_bonus: i64,
    pub referral_inviter_bonus: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

impl Config {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn deepseek_timeout(&self) -> Duration {
        Duration::from_secs(self.deepseek.timeout_seconds)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.deepseek.api_key.trim().is_empty(),
            "deepseek.api_key must not be empty"
        );
        anyhow::ensure!(
            self.quota.id_daily_limit > 0,
            "quota.id_daily_limit must be positive"
        );
        anyhow::ensure!(
            self.quota.verified_daily_limit > 0,
            "quota.verified_daily_limit must be positive"
        );
        anyhow::ensure!(
            self.quota.referral_new_user_bonus >= 0,
            "quota.referral_new_user_bonus must not be negative"
        );
        anyhow::ensure!(
            self.quota.referral_inviter_bonus >= 0,
            "quota.referral_inviter_bonus must not be negative"
        );
        anyhow::ensure!(
            self.deepseek.timeout_seconds > 0,
            "deepseek.timeout_seconds must be positive"
        );
        Ok(())
    }
}

fn default_timeout_seconds() -> u64 {
    120
}
