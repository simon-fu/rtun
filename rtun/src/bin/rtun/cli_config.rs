use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const CONFIG_ENV_VAR: &str = "RTUN_CONFIG";
pub const CONFIG_FLAG: &str = "--config";

#[derive(Debug, Clone)]
pub struct PreparedArgv {
    pub argv: Vec<String>,
    pub config_path: Option<PathBuf>,
}

pub fn prepare_argv(raw_argv: Vec<String>) -> Result<PreparedArgv> {
    if raw_argv.is_empty() {
        bail!("argv is empty");
    }

    let (base_argv, cli_config_path) = strip_config_flag(raw_argv)?;
    let base_argv = rewrite_legacy_bench_argv(base_argv);
    let config_path = match cli_config_path {
        Some(path) => Some(path),
        None => env_config_path()?,
    };

    let Some(config_path) = config_path else {
        return Ok(PreparedArgv {
            argv: base_argv,
            config_path: None,
        });
    };

    let file_cfg = load_config_file(&config_path)?;
    let merged_argv = match find_subcommand_index(&base_argv) {
        Some((idx, "relay")) => merge_config_args(base_argv, idx, file_cfg.relay, "relay")?,
        Some((idx, "socks")) => merge_config_args(base_argv, idx, file_cfg.socks, "socks")?,
        Some((idx, "shell")) => merge_config_args(base_argv, idx, file_cfg.shell, "shell")?,
        _ => base_argv,
    };

    Ok(PreparedArgv {
        argv: merged_argv,
        config_path: Some(config_path),
    })
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    relay: Option<CmdSection>,
    #[serde(default)]
    socks: Option<CmdSection>,
    #[serde(default)]
    shell: Option<CmdSection>,
}

#[derive(Debug, Default, Deserialize)]
struct CmdSection {
    #[serde(default)]
    args: Vec<String>,
}

fn strip_config_flag(raw_argv: Vec<String>) -> Result<(Vec<String>, Option<PathBuf>)> {
    let mut filtered = Vec::with_capacity(raw_argv.len());
    filtered.push(raw_argv[0].clone());

    let mut config_path = None;
    let mut idx = 1_usize;

    while idx < raw_argv.len() {
        let arg = &raw_argv[idx];

        if arg == CONFIG_FLAG {
            let path = raw_argv
                .get(idx + 1)
                .with_context(|| "missing value for --config")?;
            if path.is_empty() {
                bail!("--config value must not be empty");
            }
            if config_path.is_some() {
                bail!("--config can only be specified once");
            }
            config_path = Some(PathBuf::from(path));
            idx += 2;
            continue;
        }

        if let Some(path) = arg.strip_prefix("--config=") {
            if path.is_empty() {
                bail!("--config value must not be empty");
            }
            if config_path.is_some() {
                bail!("--config can only be specified once");
            }
            config_path = Some(PathBuf::from(path));
            idx += 1;
            continue;
        }

        filtered.push(arg.clone());
        idx += 1;
    }

    Ok((filtered, config_path))
}

fn env_config_path() -> Result<Option<PathBuf>> {
    match std::env::var(CONFIG_ENV_VAR) {
        Ok(path) => {
            if path.trim().is_empty() {
                bail!("{CONFIG_ENV_VAR} is set but empty");
            }
            Ok(Some(PathBuf::from(path)))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("{CONFIG_ENV_VAR} must be valid UTF-8")
        }
    }
}

fn load_config_file(path: &PathBuf) -> Result<ConfigFile> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read config file failed [{}]", path.display()))?;
    let cfg = toml::from_str::<ConfigFile>(&content)
        .with_context(|| format!("parse toml config failed [{}]", path.display()))?;
    Ok(cfg)
}

fn rewrite_legacy_bench_argv(mut argv: Vec<String>) -> Vec<String> {
    let Some((idx, cmd)) = find_subcommand_index(&argv) else {
        return argv;
    };
    if cmd != "bench" || idx + 1 >= argv.len() {
        return argv;
    }

    let tail = &argv[idx + 1..];
    if is_explicit_bench_subcommand(tail[0].as_str()) {
        return argv;
    }

    if tail
        .iter()
        .any(|arg| is_legacy_bench_socks_flag(arg.as_str()))
    {
        argv.insert(idx + 1, "socks".to_string());
    }
    argv
}

fn is_explicit_bench_subcommand(arg: &str) -> bool {
    matches!(arg, "socks" | "udp-server" | "udp-client")
}

fn is_legacy_bench_socks_flag(arg: &str) -> bool {
    matches!(arg, "-s" | "--socks")
        || arg.starts_with("--socks=")
        || (arg.len() > 2 && arg.starts_with("-s") && !arg.starts_with("--"))
}

#[cfg(test)]
pub(crate) fn rewrite_legacy_bench_argv_for_test(argv: Vec<String>) -> Vec<String> {
    rewrite_legacy_bench_argv(argv)
}

fn find_subcommand_index(argv: &[String]) -> Option<(usize, &str)> {
    let mut idx = 1_usize;
    while idx < argv.len() {
        let arg = argv[idx].as_str();
        if arg == "--" {
            return None;
        }
        if !arg.starts_with('-') {
            return Some((idx, arg));
        }
        idx += 1;
    }
    None
}

fn merge_config_args(
    base_argv: Vec<String>,
    subcommand_idx: usize,
    section: Option<CmdSection>,
    section_name: &str,
) -> Result<Vec<String>> {
    let config_args = match section {
        Some(section) if !section.args.is_empty() => section.args,
        _ => return Ok(base_argv),
    };

    validate_config_args(&config_args, section_name)?;

    let mut merged = Vec::with_capacity(base_argv.len() + config_args.len());
    merged.extend(base_argv[..=subcommand_idx].iter().cloned());
    merged.extend(config_args);
    merged.extend(base_argv[subcommand_idx + 1..].iter().cloned());
    Ok(merged)
}

fn validate_config_args(args: &[String], section_name: &str) -> Result<()> {
    for arg in args {
        if arg == CONFIG_FLAG || arg.starts_with("--config=") {
            bail!("{section_name}.args must not include --config");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{prepare_argv, CONFIG_ENV_VAR};
    use anyhow::{Context, Result};
    use once_cell::sync::Lazy;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[test]
    fn prepare_argv_without_config_keeps_input() -> Result<()> {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let _restore = EnvRestore::new(CONFIG_ENV_VAR);
        unsafe {
            std::env::remove_var(CONFIG_ENV_VAR);
        }

        let raw = vec![
            "rtun".to_string(),
            "relay".to_string(),
            "-L".to_string(),
            "udp://0.0.0.0:15353?to=8.8.8.8:53".to_string(),
            "https://127.0.0.1:9888".to_string(),
        ];
        let prepared = prepare_argv(raw.clone())?;
        assert_eq!(prepared.argv, raw);
        assert!(prepared.config_path.is_none());
        Ok(())
    }

    #[test]
    fn prepare_argv_inserts_relay_config_args_before_cli_args() -> Result<()> {
        let config_path = write_temp_config(
            "relay.toml",
            r#"
[relay]
args = [
  "--secret", "from-config",
  "--agent", "cfg-agent"
]
"#,
        )?;

        let raw = vec![
            "rtun".to_string(),
            "--config".to_string(),
            config_path.to_string_lossy().to_string(),
            "relay".to_string(),
            "-L".to_string(),
            "udp://0.0.0.0:15353?to=8.8.8.8:53".to_string(),
            "https://127.0.0.1:9888".to_string(),
            "--secret".to_string(),
            "from-cli".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "relay",
                "--secret",
                "from-config",
                "--agent",
                "cfg-agent",
                "-L",
                "udp://0.0.0.0:15353?to=8.8.8.8:53",
                "https://127.0.0.1:9888",
                "--secret",
                "from-cli",
            ]
        );
        assert_eq!(prepared.config_path, Some(config_path));
        Ok(())
    }

    #[test]
    fn prepare_argv_uses_env_config_when_cli_not_set() -> Result<()> {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let _restore = EnvRestore::new(CONFIG_ENV_VAR);

        let config_path = write_temp_config(
            "socks.toml",
            r#"
[socks]
args = ["--listen", "0.0.0.0:12080", "--secret", "from-config"]
"#,
        )?;
        unsafe {
            std::env::set_var(CONFIG_ENV_VAR, &config_path);
        }

        let raw = vec![
            "rtun".to_string(),
            "socks".to_string(),
            "https://127.0.0.1:9888".to_string(),
            "--secret".to_string(),
            "from-cli".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "socks",
                "--listen",
                "0.0.0.0:12080",
                "--secret",
                "from-config",
                "https://127.0.0.1:9888",
                "--secret",
                "from-cli",
            ]
        );
        assert_eq!(prepared.config_path, Some(config_path));
        Ok(())
    }

    #[test]
    fn prepare_argv_prefers_cli_config_over_env_config() -> Result<()> {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let _restore = EnvRestore::new(CONFIG_ENV_VAR);

        let env_config_path = write_temp_config(
            "env.toml",
            r#"
[socks]
args = ["--listen", "127.0.0.1:12080"]
"#,
        )?;
        let cli_config_path = write_temp_config(
            "cli.toml",
            r#"
[socks]
args = ["--listen", "0.0.0.0:13080"]
"#,
        )?;
        unsafe {
            std::env::set_var(CONFIG_ENV_VAR, &env_config_path);
        }

        let raw = vec![
            "rtun".to_string(),
            format!("--config={}", cli_config_path.display()),
            "socks".to_string(),
            "https://127.0.0.1:9888".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "socks",
                "--listen",
                "0.0.0.0:13080",
                "https://127.0.0.1:9888",
            ]
        );
        assert_eq!(prepared.config_path, Some(cli_config_path));
        Ok(())
    }

    #[test]
    fn prepare_argv_supports_config_equals_syntax() -> Result<()> {
        let config_path = write_temp_config(
            "relay-eq.toml",
            r#"
[relay]
args = ["--agent", "eq-agent"]
"#,
        )?;
        let raw = vec![
            "rtun".to_string(),
            format!("--config={}", config_path.display()),
            "relay".to_string(),
            "-L".to_string(),
            "udp://0.0.0.0:15353?to=8.8.8.8:53".to_string(),
            "https://127.0.0.1:9888".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "relay",
                "--agent",
                "eq-agent",
                "-L",
                "udp://0.0.0.0:15353?to=8.8.8.8:53",
                "https://127.0.0.1:9888",
            ]
        );
        assert_eq!(prepared.config_path, Some(config_path));
        Ok(())
    }

    #[test]
    fn prepare_argv_inserts_shell_config_args_before_cli_args() -> Result<()> {
        let config_path = write_temp_config(
            "shell.toml",
            r#"
[shell]
args = ["--agent", "cfg-shell-agent", "--secret", "from-config"]
"#,
        )?;

        let raw = vec![
            "rtun".to_string(),
            "--config".to_string(),
            config_path.to_string_lossy().to_string(),
            "shell".to_string(),
            "https://127.0.0.1:9888".to_string(),
            "--secret".to_string(),
            "from-cli".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "shell",
                "--agent",
                "cfg-shell-agent",
                "--secret",
                "from-config",
                "https://127.0.0.1:9888",
                "--secret",
                "from-cli",
            ]
        );
        assert_eq!(prepared.config_path, Some(config_path));
        Ok(())
    }

    #[test]
    fn prepare_argv_rejects_config_argument_inside_toml_args() -> Result<()> {
        let config_path = write_temp_config(
            "relay-invalid.toml",
            r#"
[relay]
args = ["--config", "/tmp/other.toml"]
"#,
        )?;
        let raw = vec![
            "rtun".to_string(),
            "--config".to_string(),
            config_path.to_string_lossy().to_string(),
            "relay".to_string(),
            "-L".to_string(),
            "udp://0.0.0.0:15353?to=8.8.8.8:53".to_string(),
            "https://127.0.0.1:9888".to_string(),
        ];
        let err = prepare_argv(raw).unwrap_err();
        assert!(
            err.to_string().contains("must not include --config"),
            "{err:#}"
        );
        Ok(())
    }

    #[test]
    fn prepare_argv_rewrites_legacy_bench_to_socks_subcommand() -> Result<()> {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let _restore = EnvRestore::new(CONFIG_ENV_VAR);
        unsafe {
            std::env::remove_var(CONFIG_ENV_VAR);
        }

        let raw = vec![
            "rtun".to_string(),
            "bench".to_string(),
            "-s".to_string(),
            "127.0.0.1:51080".to_string(),
            "-a".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            "12345".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "bench",
                "socks",
                "-s",
                "127.0.0.1:51080",
                "-a",
                "127.0.0.1",
                "-p",
                "12345",
            ]
        );
        Ok(())
    }

    #[test]
    fn prepare_argv_rewrites_legacy_bench_with_socks_long_flag() -> Result<()> {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let _restore = EnvRestore::new(CONFIG_ENV_VAR);
        unsafe {
            std::env::remove_var(CONFIG_ENV_VAR);
        }

        let raw = vec![
            "rtun".to_string(),
            "bench".to_string(),
            "--socks".to_string(),
            "127.0.0.1:51080".to_string(),
            "-a".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            "12345".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "bench",
                "socks",
                "--socks",
                "127.0.0.1:51080",
                "-a",
                "127.0.0.1",
                "-p",
                "12345",
            ]
        );
        Ok(())
    }

    #[test]
    fn prepare_argv_rewrites_legacy_bench_with_config_flag() -> Result<()> {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let _restore = EnvRestore::new(CONFIG_ENV_VAR);
        unsafe {
            std::env::remove_var(CONFIG_ENV_VAR);
        }

        let config_path = write_temp_config("bench-empty.toml", "")?;
        let raw = vec![
            "rtun".to_string(),
            "--config".to_string(),
            config_path.to_string_lossy().to_string(),
            "bench".to_string(),
            "-s".to_string(),
            "127.0.0.1:51080".to_string(),
            "-a".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            "12345".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec![
                "rtun",
                "bench",
                "socks",
                "-s",
                "127.0.0.1:51080",
                "-a",
                "127.0.0.1",
                "-p",
                "12345",
            ]
        );
        assert_eq!(prepared.config_path, Some(config_path));
        Ok(())
    }

    #[test]
    fn prepare_argv_does_not_rewrite_bench_listen_flag_to_socks() -> Result<()> {
        let _guard = ENV_LOCK.lock().expect("lock env");
        let _restore = EnvRestore::new(CONFIG_ENV_VAR);
        unsafe {
            std::env::remove_var(CONFIG_ENV_VAR);
        }

        let raw = vec![
            "rtun".to_string(),
            "bench".to_string(),
            "--listen".to_string(),
            "0.0.0.0:9001".to_string(),
        ];
        let prepared = prepare_argv(raw)?;
        assert_eq!(
            prepared.argv,
            vec!["rtun", "bench", "--listen", "0.0.0.0:9001",]
        );
        Ok(())
    }

    fn write_temp_config(file_hint: &str, content: &str) -> Result<PathBuf> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .with_context(|| "clock before unix epoch")?
            .as_nanos();
        let pid = std::process::id();
        let name = format!("rtun-cli-config-{pid}-{ts}-{file_hint}");
        let path = std::env::temp_dir().join(name);
        fs::write(&path, content)
            .with_context(|| format!("write temp config failed [{}]", display_path(&path)))?;
        Ok(path)
    }

    fn display_path(path: &Path) -> String {
        path.to_string_lossy().to_string()
    }

    struct EnvRestore {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvRestore {
        fn new(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            Self { key, old }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => unsafe {
                    std::env::set_var(self.key, v);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }
}
