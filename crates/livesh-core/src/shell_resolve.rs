use std::{
    env,
    ffi::OsString,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};

pub fn resolve_real_shell() -> anyhow::Result<PathBuf> {
    resolve_real_shell_with_config(None)
}

pub fn resolve_real_shell_with_config(configured_shell: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(shell) = non_empty_env("LIVESH_REAL_SHELL") {
        return Ok(PathBuf::from(shell));
    }

    if let Some(shell) = configured_shell.filter(|shell| !shell.trim().is_empty()) {
        return Ok(PathBuf::from(shell));
    }

    if let Some(shell) = non_empty_env("SHELL") {
        let candidate = PathBuf::from(shell);
        if !is_livesh_binary(&candidate) {
            return Ok(candidate);
        }
    }

    for candidate in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    bail!("could not resolve a real shell");
}

pub fn exec_real_shell() -> anyhow::Result<()> {
    let shell = resolve_real_shell()?;
    let err = Command::new(&shell).exec();
    Err(err).with_context(|| format!("exec {}", shell.display()))
}

pub fn filtered_current_env() -> Vec<(String, String)> {
    env::vars()
        .filter(|(key, _)| !key.starts_with("LIVESH_INTERNAL_"))
        .collect()
}

pub fn env_for_command(envs: &[(String, String)]) -> Vec<(OsString, OsString)> {
    envs.iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn is_livesh_binary(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == "livesh")
}

#[cfg(test)]
mod tests {
    #[test]
    fn filters_internal_env_names() {
        let env = vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("LIVESH_INTERNAL_X".to_string(), "1".to_string()),
        ];
        let filtered: Vec<_> = env
            .into_iter()
            .filter(|(key, _)| !key.starts_with("LIVESH_INTERNAL_"))
            .collect();
        assert_eq!(filtered, vec![("PATH".to_string(), "/bin".to_string())]);
    }
}
