use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde::{Deserialize, Serialize};
use std::{fs, io::ErrorKind, path::PathBuf};
use zcash_voting::config::{
    decide_config_switch, resolve_dynamic_voting_config_from_attempts,
    resolve_static_voting_config, AuthenticatedRound, PinnedConfigSource,
};
use zcash_voting::wire::{
    DynamicConfigAttempt, DynamicConfigMirrorFailure, ResolveVotingConfigOptions,
    ResolvedVotingConfig, ResolvedVotingConfigSummary,
};

type RequestBody = Full<Bytes>;
type HyperClient = Client<HttpsConnector<HttpConnector>, RequestBody>;

#[tokio::main]
async fn main() -> Result<()> {
    let command = Command::parse(std::env::args().skip(1))?;
    let fetcher = DirectHttpsFetcher::new();

    match command {
        Command::Resolve { source } => {
            let (resolved, skipped_config_urls) = fetch_resolved_config(&fetcher, &source).await?;
            print_resolved_config(&resolved, &skipped_config_urls);
        }
        Command::CheckSwitch { state_path, source } => {
            let previous = read_state(&state_path)?;
            let (resolved, skipped_config_urls) = fetch_resolved_config(&fetcher, &source).await?;
            let next_summary = ResolvedVotingConfigSummary::from(&resolved);
            let decision = decide_config_switch(
                previous.as_ref().map(|state| state.summary.clone()),
                next_summary.clone(),
            );

            print_resolved_config(&resolved, &skipped_config_urls);
            println!("switch kind: {:?}", decision.kind);
            println!("state path: {}", state_path.display());

            let next_state = StoredConfigState {
                summary: next_summary,
                dynamic_config_fingerprint: resolved.dynamic_config_fingerprint,
                authenticated_rounds: resolved.authenticated_rounds,
            };
            write_state(&state_path, &next_state)?;
            println!("state written: {}", state_path.display());
        }
    }

    Ok(())
}

enum Command {
    Resolve { source: String },
    CheckSwitch { state_path: PathBuf, source: String },
}

impl Command {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let args = args.collect::<Vec<_>>();
        match args.as_slice() {
            [source] => Ok(Self::Resolve {
                source: source.clone(),
            }),
            [command, state_path, source] if command == "check-switch" => Ok(Self::CheckSwitch {
                state_path: PathBuf::from(state_path),
                source: source.clone(),
            }),
            _ => Err(anyhow!(
                "usage:\n  cargo run -p zcash_voting --example config_fetcher -- \
                 <static-source-url>\n  cargo run -p zcash_voting --example \
                 config_fetcher -- check-switch <state-json> <static-source-url>"
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredConfigState {
    summary: ResolvedVotingConfigSummary,
    dynamic_config_fingerprint: String,
    authenticated_rounds: Vec<AuthenticatedRound>,
}

/// Fetches and resolves a config, walking the static config's dynamic mirrors.
///
/// A v1 static config names one mirror, so this is a single fetch. A v2 static
/// config names an ordered list, and each mirror is tried in turn until one
/// both fetches and authenticates.
async fn fetch_resolved_config(
    fetcher: &DirectHttpsFetcher,
    source: &str,
) -> Result<(ResolvedVotingConfig, Vec<DynamicConfigMirrorFailure>)> {
    let static_url = PinnedConfigSource::parse(source)
        .map_err(|e| anyhow!("parse static config source failed: {e}"))?
        .url;
    let static_bytes = fetcher.fetch_bytes(&static_url).await?;
    let resolved_static = resolve_static_voting_config(source, &static_bytes)
        .map_err(|e| anyhow!("resolve static config failed: {e}"))?;

    let urls = resolved_static.dynamic_config_urls.clone();
    let mut attempts = Vec::new();
    let mut best = None;

    for url in urls.iter() {
        attempts.push(match fetcher.fetch_bytes(url).await {
            Ok(bytes) => DynamicConfigAttempt::fetched(url.clone(), bytes),
            Err(e) => DynamicConfigAttempt::failed(url.clone(), format!("{e:#}")),
        });

        // Re-resolve the attempts gathered so far, so the resolver's own
        // preference rules decide the winner. A config that authenticates
        // rounds ends the walk; a round-less one is kept but the next mirror
        // is still tried.
        match resolve_dynamic_voting_config_from_attempts(
            resolved_static.clone(),
            attempts.clone(),
            ResolveVotingConfigOptions::default(),
        ) {
            Ok(outcome) => {
                let has_rounds = !outcome.0.authenticated_rounds.is_empty();
                best = Some(outcome);
                if has_rounds {
                    break;
                }
            }
            Err(e) if attempts.len() == urls.len() && best.is_none() => {
                return Err(anyhow!("resolve voting config failed: {e}"))
            }
            Err(_) => {}
        }
    }

    best.ok_or_else(|| anyhow!("resolve voting config failed: no dynamic config mirrors to try"))
}

fn print_resolved_config(
    resolved: &ResolvedVotingConfig,
    skipped_config_urls: &[DynamicConfigMirrorFailure],
) {
    for failure in skipped_config_urls {
        println!("skipped mirror {}: {}", failure.url, failure.reason);
    }
    println!("vote servers: {}", resolved.vote_servers.len());
    println!("PIR endpoints: {}", resolved.pir_endpoints.len());
    println!(
        "PIR layout: depth={} tier0={} tier1={} poly_len={}",
        resolved.pir_layout.pir_depth,
        resolved.pir_layout.tier0_layers,
        resolved.pir_layout.tier1_layers,
        resolved.pir_layout.poly_len
    );
    println!(
        "authenticated rounds: {}",
        resolved.authenticated_rounds.len()
    );
}

fn read_state(state_path: &PathBuf) -> Result<Option<StoredConfigState>> {
    match fs::read(state_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("decode {}", state_path.display()))
            .map(Some),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", state_path.display())),
    }
}

fn write_state(state_path: &PathBuf, state: &StoredConfigState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("encode config state")?;
    fs::write(state_path, bytes).with_context(|| format!("write {}", state_path.display()))
}

struct DirectHttpsFetcher {
    client: HyperClient,
}

impl DirectHttpsFetcher {
    fn new() -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .enable_http2()
            .wrap_connector(connector);
        let client = Client::builder(TokioExecutor::new()).build(https);
        Self { client }
    }

    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(url)
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            .body(Full::new(Bytes::new()))
            .context("build config request")?;
        let response = self
            .client
            .request(request)
            .await
            .context("send config request")?;
        if !response.status().is_success() {
            return Err(anyhow!("config fetch returned HTTP {}", response.status()));
        }
        Ok(response
            .into_body()
            .collect()
            .await
            .context("read config response body")?
            .to_bytes()
            .to_vec())
    }
}
