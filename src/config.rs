// SPEC-MANAGED: tech-design/semantic/cap-src.md#schema
// CODEGEN-BEGIN
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub protect: Protect,
    /// Legacy priority settings are accepted but no longer applied or emitted.
    #[serde(default, skip_serializing)]
    pub defaults: Defaults,
    #[serde(default)]
    pub log: Log,
}

/// Timeout settings and parse-only compatibility for retired resource policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Protect {
    /// Retired memory floor. Accepted for compatibility; ignored at runtime.
    #[serde(skip_serializing)]
    pub min_free_gb: f64,
    /// Retired pause threshold. The old key is also accepted.
    #[serde(alias = "max_used_percent", skip_serializing)]
    pub pause_used_percent: u32,
    /// Retired kill threshold. Ignored at runtime.
    #[serde(skip_serializing)]
    pub kill_used_percent: u32,
    /// Retired CPU pause threshold. Ignored at runtime.
    #[serde(skip_serializing)]
    pub pause_load_percent: u32,
    /// Grace in seconds after an explicit timeout sends SIGTERM.
    /// Zero sends SIGKILL to the process group immediately.
    pub kill_grace_secs: u64,
    /// Retired pressure eviction setting. Ignored at runtime.
    #[serde(skip_serializing)]
    pub kill_all_paused_after_ticks: u32,
    /// Interval in milliseconds between timeout checks and optional telemetry.
    pub sample_interval_ms: u64,
    /// Retired pressure debounce setting. Ignored at runtime.
    #[serde(skip_serializing)]
    pub trigger_samples: u32,
    /// Retired process reaping setting. Ignored even if an old file enables it.
    #[serde(skip_serializing)]
    pub reap_enabled: bool,
    /// Retired process reaping setting. Ignored at runtime.
    #[serde(skip_serializing)]
    pub reap_min_uptime_secs: u64,
    /// Retired process reaping setting. Ignored at runtime.
    #[serde(skip_serializing)]
    pub reap_cooldown_secs: u64,
    /// Default absolute timeout in seconds. Zero disables it.
    pub default_timeout_secs: u64,
    /// Default idle timeout in seconds. Zero disables it.
    pub default_idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    /// Retired priority setting. Commands always inherit caller priority.
    #[serde(skip_serializing)]
    pub nice: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Log {
    /// Append completed external command records to the daily JSONL run log.
    pub enabled: bool,
}

impl Default for Protect {
    fn default() -> Self {
        Self {
            min_free_gb: 0.0,
            pause_used_percent: 0,
            kill_used_percent: 0,
            pause_load_percent: 0,
            kill_grace_secs: 3,
            kill_all_paused_after_ticks: 0,
            sample_interval_ms: 500,
            trigger_samples: 0,
            reap_enabled: false,
            reap_min_uptime_secs: 0,
            reap_cooldown_secs: 0,
            default_timeout_secs: 0,
            default_idle_timeout_secs: 0,
        }
    }
}

impl Default for Log {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// @spec tech-design/semantic/cap-src.md#schema
impl Config {
    pub fn load() -> Result<Self> {
        let path = paths::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        paths::ensure_home()?;
        let path = paths::config_path()?;
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_controls_and_timeouts_are_disabled_by_default() {
        let cfg = Config::default();
        assert_eq!(cfg.protect.default_timeout_secs, 0);
        assert_eq!(cfg.protect.default_idle_timeout_secs, 0);
        assert!(!cfg.protect.reap_enabled);
        assert_eq!(cfg.defaults.nice, 0);
    }

    #[test]
    fn legacy_resource_config_is_readable_but_not_advertised() {
        let cfg: Config = toml::from_str(
            r#"
            [protect]
            min_free_gb = 99999.0
            max_used_percent = 80
            kill_used_percent = 10
            pause_load_percent = 1
            reap_enabled = true
            trigger_samples = 1
            kill_all_paused_after_ticks = 1
            reap_min_uptime_secs = 1
            reap_cooldown_secs = 1
            [defaults]
            nice = 19
        "#,
        )
        .unwrap();
        assert_eq!(cfg.protect.min_free_gb, 99999.0);
        assert_eq!(cfg.protect.pause_used_percent, 80);
        assert_eq!(cfg.defaults.nice, 19);
        let output = toml::to_string(&cfg).unwrap();
        for retired in [
            "min_free_gb",
            "max_used_percent",
            "pause_used_percent",
            "kill_used_percent",
            "pause_load_percent",
            "reap_enabled",
            "trigger_samples",
            "kill_all_paused_after_ticks",
            "reap_min_uptime_secs",
            "reap_cooldown_secs",
            "defaults",
            "nice",
        ] {
            assert!(
                !output.contains(retired),
                "retired setting {retired} must not be emitted: {output}"
            );
        }
        assert!(output.contains("default_timeout_secs = 0"));
        assert!(output.contains("kill_grace_secs = 3"));
    }

    #[test]
    fn timeout_and_log_settings_round_trip() {
        let mut original = Config::default();
        original.protect.default_timeout_secs = 30;
        original.protect.default_idle_timeout_secs = 10;
        original.protect.kill_grace_secs = 2;
        original.protect.sample_interval_ms = 100;
        original.log.enabled = false;
        let text = toml::to_string(&original).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.protect.default_timeout_secs, 30);
        assert_eq!(parsed.protect.default_idle_timeout_secs, 10);
        assert_eq!(parsed.protect.kill_grace_secs, 2);
        assert_eq!(parsed.protect.sample_interval_ms, 100);
        assert!(!parsed.log.enabled);
    }

    #[test]
    fn empty_legacy_sections_use_current_defaults() {
        let cfg: Config = toml::from_str("[protect]\n[defaults]\n").unwrap();
        assert!(!cfg.protect.reap_enabled);
        assert_eq!(cfg.defaults.nice, 0);
        assert_eq!(cfg.protect.sample_interval_ms, 500);
    }
}
// CODEGEN-END
