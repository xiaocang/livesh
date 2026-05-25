use crate::limits::Limits;

#[derive(Debug, Clone)]
pub struct Config {
    pub default_name: String,
    pub real_shell: String,
    pub limits: Limits,
    pub cleanup_lost_on_startup: bool,
    pub detached_idle_ttl_secs: u64,
    pub single_attached_client: bool,
    pub open_steals_existing_attach: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_name: "shell".to_string(),
            real_shell: String::new(),
            limits: Limits::default(),
            cleanup_lost_on_startup: true,
            detached_idle_ttl_secs: 0,
            single_attached_client: true,
            open_steals_existing_attach: true,
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };
        if !path.exists() {
            return Ok(Self::default());
        }

        let text = std::fs::read_to_string(&path)?;
        Ok(parse_config(&text))
    }
}

fn config_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(std::path::PathBuf::from(dir).join("livesh/config.toml"));
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".config/livesh/config.toml"))
}

fn parse_config(text: &str) -> Config {
    let mut config = Config::default();
    let mut section = String::new();

    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = name.trim().to_string();
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match (section.as_str(), key) {
            ("shell", "default_name") => config.default_name = parse_string(value),
            ("shell", "real_shell") => config.real_shell = parse_string(value),
            ("limits", "max_shells") => assign_usize(value, &mut config.limits.max_shells),
            ("limits", "scrollback_lines_per_shell") => {
                assign_usize(value, &mut config.limits.scrollback_lines_per_shell)
            }
            ("limits", "scrollback_bytes_per_shell") => {
                assign_usize(value, &mut config.limits.scrollback_bytes_per_shell)
            }
            ("limits", "event_ring_bytes_per_shell") => {
                assign_usize(value, &mut config.limits.event_ring_bytes_per_shell)
            }
            ("limits", "snapshot_bytes_per_shell") => {
                assign_usize(value, &mut config.limits.snapshot_bytes_per_shell)
            }
            ("limits", "global_runtime_bytes") => {
                if let Ok(parsed) = value.parse() {
                    config.limits.global_runtime_bytes = parsed;
                }
            }
            ("gc", "cleanup_lost_on_startup") => {
                assign_bool(value, &mut config.cleanup_lost_on_startup)
            }
            ("gc", "detached_idle_ttl_secs") => {
                if let Ok(parsed) = value.parse() {
                    config.detached_idle_ttl_secs = parsed;
                }
            }
            ("attach", "single_attached_client") => {
                assign_bool(value, &mut config.single_attached_client)
            }
            ("attach", "open_steals_existing_attach") => {
                assign_bool(value, &mut config.open_steals_existing_attach)
            }
            _ => {}
        }
    }

    config
}

fn parse_string(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

fn assign_usize(value: &str, target: &mut usize) {
    if let Ok(parsed) = value.parse() {
        *target = parsed;
    }
}

fn assign_bool(value: &str, target: &mut bool) {
    if let Ok(parsed) = value.parse() {
        *target = parsed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_config_values() {
        let config = parse_config(
            r#"
            [shell]
            default_name = "dev"
            real_shell = "/bin/sh"

            [limits]
            max_shells = 2
            event_ring_bytes_per_shell = 128

            [attach]
            open_steals_existing_attach = false
            "#,
        );

        assert_eq!(config.default_name, "dev");
        assert_eq!(config.real_shell, "/bin/sh");
        assert_eq!(config.limits.max_shells, 2);
        assert_eq!(config.limits.event_ring_bytes_per_shell, 128);
        assert!(!config.open_steals_existing_attach);
    }
}
