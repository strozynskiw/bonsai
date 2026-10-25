//! Operator-only configuration; HTTP requests cannot alter these policies.
use anyhow::{Context, Result, ensure};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Listen(Config),
    Mint {
        output: PathBuf,
        ttl: u64,
        sessions: Vec<i64>,
        scopes: Vec<String>,
    },
    Revoke {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub bind: SocketAddr,
    pub allow_remote: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub origins: Vec<String>,
    pub hosts: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            allow_remote: false,
            cert: None,
            key: None,
            origins: Vec::new(),
            hosts: Vec::new(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.cert.is_some() == self.key.is_some(),
            "TLS requires both certificate and key"
        );
        ensure!(
            self.bind.ip().is_loopback() || (self.allow_remote && self.cert.is_some()),
            "Non-loopback bind requires --allow-remote and TLS certificate/key"
        );
        for origin in &self.origins {
            let uri: http::Uri = origin.parse().context("Invalid origin")?;
            ensure!(
                matches!(uri.scheme_str(), Some("http" | "https"))
                    && uri.authority().is_some()
                    && uri.path_and_query().is_none_or(|path| path.as_str() == "/")
                    && !origin.ends_with('/'),
                "Origin must be an exact scheme://authority"
            );
        }
        for host in &self.hosts {
            let authority: http::uri::Authority = host.parse().context("Invalid allowed Host")?;
            ensure!(!authority.as_str().contains('@'), "Invalid allowed Host");
        }
        Ok(())
    }
}

pub(crate) fn parse(args: &[String]) -> Result<Command> {
    if args.first().is_some_and(|value| value == "token") {
        return parse_token(&args[1..]);
    }
    let mut config = Config::default();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--allow-remote" {
            config.allow_remote = true;
            continue;
        }
        let value = args.next().context("Missing serve option value")?;
        match flag.as_str() {
            "--bind" => config.bind = value.parse().context("--bind requires a literal IP:port")?,
            "--tls-cert" => config.cert = Some(value.into()),
            "--tls-key" => config.key = Some(value.into()),
            "--allow-origin" => config.origins.push(value.clone()),
            "--allow-host" => config.hosts.push(value.clone()),
            _ => anyhow::bail!("Unknown serve option: {flag}"),
        }
    }
    config.validate()?;
    Ok(Command::Listen(config))
}
fn parse_token(args: &[String]) -> Result<Command> {
    if let [verb, id] = args
        && verb == "revoke"
    {
        return Ok(Command::Revoke { id: id.clone() });
    }
    ensure!(
        args.first().is_some_and(|v| v == "mint"),
        "Usage: bonsai serve token mint --output PATH [--ttl SECONDS] [--scope read|control|respond|artifacts] [--session ID]; token revoke ID"
    );
    let mut output = None;
    let mut ttl = 3600;
    let mut scopes = Vec::new();
    let mut sessions = Vec::new();
    let mut args = args[1..].iter();
    while let Some(flag) = args.next() {
        let value = args.next().context("Missing token option value")?;
        match flag.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--ttl" => ttl = value.parse()?,
            "--scope" => {
                ensure!(
                    matches!(value.as_str(), "read" | "control" | "respond" | "artifacts"),
                    "Invalid scope"
                );
                scopes.push(value.clone());
            }
            "--session" => {
                let id: i64 = value.parse()?;
                ensure!(id > 0, "Invalid session");
                sessions.push(id);
            }
            _ => anyhow::bail!("Unknown token option: {flag}"),
        }
    }
    ensure!(
        ttl > 0 && ttl <= 86400,
        "Token TTL must be 1..=86400 seconds"
    );
    if scopes.is_empty() {
        scopes = ["read", "control", "respond", "artifacts"]
            .map(String::from)
            .to_vec();
    }
    Ok(Command::Mint {
        output: output.context("--output is required")?,
        ttl,
        sessions,
        scopes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_requires_both_opt_in_and_tls() {
        let mut config = Config {
            bind: "0.0.0.0:0".parse().unwrap(),
            ..Config::default()
        };
        assert!(config.validate().is_err());
        config.allow_remote = true;
        assert!(config.validate().is_err());
        config.cert = Some("cert".into());
        assert!(config.validate().is_err());
        config.key = Some("key".into());
        assert!(config.validate().is_ok());
    }
}
