use anyhow::{Context, Result, anyhow, bail};
use dialoguer::{Input, Password};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{IsTerminal, stdin, stdout};
use std::path::PathBuf;

const DEFAULT_URL: &str = "http://localhost:8080/api";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientConfig {
    pub url: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub url: String,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSummary {
    pub path: String,
    pub url: Option<String>,
    pub token_state: &'static str,
}

pub fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("could not determine config directory"))?;
    Ok(base.join("aurcache-client").join("config.json"))
}

pub fn load_config() -> Result<ClientConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(ClientConfig::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

pub fn save_config(config: &ClientConfig) -> Result<()> {
    let path = config_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid config path {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    let body = serde_json::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(&path, format!("{body}\n"))
        .with_context(|| format!("failed to write config file {}", path.display()))
}

pub fn resolve_runtime_config(
    cli_url: Option<String>,
    cli_token: Option<String>,
) -> Result<RuntimeConfig> {
    let mut config = load_config()?;
    let (url, url_prompted) = resolve_url(cli_url, &mut config)?;
    let (token, token_prompted) = resolve_token(cli_token, &mut config)?;
    if url_prompted || token_prompted {
        save_config(&config)?;
    }
    Ok(RuntimeConfig {
        url,
        token: token.filter(|token| !token.is_empty()),
    })
}

pub fn summarize_config(config: &ClientConfig) -> Result<ConfigSummary> {
    Ok(ConfigSummary {
        path: config_path()?.display().to_string(),
        url: config.url.clone(),
        token_state: token_state(config.token.as_deref()),
    })
}

pub fn set_url(mut config: ClientConfig, url: Option<String>) -> Result<ClientConfig> {
    let url = match normalize_url(url) {
        Some(url) => url,
        None => {
            ensure_interactive("AURCache URL")?;
            prompt_for_url()?
        }
    };
    config.url = Some(url);
    Ok(config)
}

pub fn set_token(mut config: ClientConfig, token: Option<String>) -> Result<ClientConfig> {
    let token = match token {
        Some(token) => token.trim().to_string(),
        None => {
            ensure_interactive("AURCache token")?;
            prompt_for_token()?
        }
    };
    config.token = Some(token);
    Ok(config)
}

/// Whether both stdin and stdout are attached to a terminal, i.e. whether
/// it's safe to prompt the user interactively.
pub fn is_interactive() -> bool {
    stdin().is_terminal() && stdout().is_terminal()
}

/// Prompts for a new API token and persists it to the config file.
///
/// This reuses the same prompt as `set_token`/initial resolution (via
/// `prompt_for_token`), so a user who never configured a token and one whose
/// stored token was rejected (401) get the identical prompt/save behavior.
/// Used when a request fails authentication and we want to give the user a
/// chance to fix their credentials without re-running the command from
/// scratch.
pub fn prompt_and_save_token(config: ClientConfig) -> Result<String> {
    let config = set_token(config, None)?;
    save_config(&config)?;
    Ok(config.token.unwrap_or_default())
}

fn prompt_for_url() -> Result<String> {
    let url: String = Input::new()
        .with_prompt("AURCache URL (including /api)")
        .default(DEFAULT_URL.to_string())
        .interact_text()
        .context("failed to read AURCache URL")?;

    normalize_url(Some(url)).ok_or_else(|| anyhow!("AURCache URL cannot be empty"))
}

fn prompt_for_token() -> Result<String> {
    Password::new()
        .with_prompt("AURCache API token (leave empty if auth is disabled)")
        .allow_empty_password(true)
        .interact()
        .context("failed to read AURCache token")
}

fn resolve_url(cli_url: Option<String>, config: &mut ClientConfig) -> Result<(String, bool)> {
    if let Some(url) = normalize_url(cli_url).or_else(|| normalize_url(config.url.clone())) {
        return Ok((url, false));
    }

    ensure_interactive("AURCache URL")?;
    let prompted = prompt_for_url()?;
    config.url = Some(prompted.clone());
    Ok((prompted, true))
}

fn resolve_token(
    cli_token: Option<String>,
    config: &mut ClientConfig,
) -> Result<(Option<String>, bool)> {
    match cli_token {
        Some(token) => Ok((Some(token.trim().to_string()), false)),
        None => match config.token.clone() {
            Some(token) => Ok((Some(token.trim().to_string()), false)),
            None => {
                ensure_interactive("AURCache token")?;
                let prompted = prompt_for_token()?;
                config.token = Some(prompted.clone());
                Ok((Some(prompted), true))
            }
        },
    }
}

fn normalize_url(url: Option<String>) -> Option<String> {
    url.map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
}

fn token_state(token: Option<&str>) -> &'static str {
    match token {
        None => "not_set",
        Some("") => "empty",
        Some(_) => "set",
    }
}

fn ensure_interactive(field_name: &str) -> Result<()> {
    if stdin().is_terminal() && stdout().is_terminal() {
        Ok(())
    } else {
        bail!(
            "missing {field_name}; set it with CLI flags, environment variables, or `aurcache-cli config`"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientConfig, set_token, summarize_config};

    #[test]
    fn explicit_empty_token_is_preserved() {
        let config = set_token(ClientConfig::default(), Some(String::new())).unwrap();
        assert_eq!(config.token.as_deref(), Some(""));
    }

    #[test]
    fn summary_marks_empty_token() {
        let summary = summarize_config(&ClientConfig {
            url: Some("http://localhost:8080/api".to_string()),
            token: Some(String::new()),
        })
        .unwrap();
        assert_eq!(summary.token_state, "empty");
    }
}
