//! Device registration and trust proof helpers.

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use reqwest::Client as HttpClient;

use crate::{
    config::EdgeConfig,
    contracts::{
        DeviceRegistrationTrustProof, RegisterDeviceRequest, RegistrationChallengeResponse,
    },
    discovery::{discover_repositories, discover_repository_catalog},
};

pub(crate) fn print_trust_keypair() {
    let mut private_key = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut private_key);
    let signing_key = SigningKey::from_bytes(&private_key);

    println!(
        "{}",
        serde_json::json!({
            "private_key": URL_SAFE_NO_PAD.encode(private_key),
            "public_key": URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes()),
        })
    );
}

pub(crate) async fn wait_for_registration(http: &HttpClient, config: &EdgeConfig) {
    loop {
        match register_device(http, config).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!(error = %error, "initial device registration failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

pub(crate) async fn register_device(http: &HttpClient, config: &EdgeConfig) -> anyhow::Result<()> {
    let discovered_repos = discover_repositories(
        &config.allowed_repo_roots,
        &config.excluded_repo_paths,
    )
    .context("failed to discover repositories from configured roots")?;
    let repositories = discover_repository_catalog(
        &config.allowed_repo_roots,
        &config.excluded_repo_paths,
    )
    .context("failed to discover repository catalog from configured roots")?;
    let trust = build_registration_trust_proof(http, config).await?;
    let response = http
        .put(format!(
            "{}/api/v1/devices/{}",
            config.api_url, config.device_id
        ))
        .json(&RegisterDeviceRequest {
            name: config.device_name.clone(),
            primary_flag: config.primary_flag,
            allowed_repos: config.allowed_repos.clone(),
            allowed_repo_roots: config
                .allowed_repo_roots
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
            hidden_repos: config.hidden_repos.clone(),
            excluded_repo_paths: config
                .excluded_repo_paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
            discovered_repos,
            repositories,
            capabilities: config.capabilities.clone(),
            trust,
        })
        .send()
        .await
        .context("failed to send device registration")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("device registration failed with status {status}: {body}");
    }

    Ok(())
}

async fn build_registration_trust_proof(
    http: &HttpClient,
    config: &EdgeConfig,
) -> anyhow::Result<Option<DeviceRegistrationTrustProof>> {
    let Some(orchestrator_public_key) = config.orchestrator_public_key.as_deref() else {
        return Ok(None);
    };
    let Some(edge_signing_key) = config.edge_signing_key.as_deref() else {
        anyhow::bail!(
            "ELOWEN_EDGE_SIGNING_KEY is required when ELOWEN_ORCHESTRATOR_PUBLIC_KEY is configured"
        );
    };

    let pinned_orchestrator_key =
        decode_verifying_key(orchestrator_public_key, "orchestrator public key")?;
    let edge_signing_key = decode_signing_key(edge_signing_key, "edge signing key")?;
    let challenge = fetch_registration_challenge(http, config).await?;

    if challenge.orchestrator_public_key != orchestrator_public_key.trim() {
        anyhow::bail!("registration challenge was signed by an unexpected orchestrator public key");
    }

    let challenge_signature =
        decode_signature(&challenge.signature, "orchestrator challenge signature")?;
    let challenge_payload = orchestrator_challenge_payload(
        &challenge.challenge_id,
        &challenge.challenge,
        challenge.issued_at,
    );
    pinned_orchestrator_key
        .verify(challenge_payload.as_bytes(), &challenge_signature)
        .context("failed to verify orchestrator registration challenge signature")?;

    let edge_public_key = URL_SAFE_NO_PAD.encode(edge_signing_key.verifying_key().to_bytes());
    let registration_payload = edge_registration_payload(
        &config.device_id,
        &config.device_name,
        config.primary_flag,
        &challenge.challenge_id,
        &challenge.challenge,
        challenge.issued_at,
        &edge_public_key,
    );
    let edge_signature = edge_signing_key.sign(registration_payload.as_bytes());

    Ok(Some(DeviceRegistrationTrustProof {
        orchestrator_challenge_id: challenge.challenge_id,
        orchestrator_challenge: challenge.challenge,
        orchestrator_challenge_issued_at: challenge.issued_at,
        orchestrator_signature: challenge.signature,
        edge_public_key,
        edge_signature: URL_SAFE_NO_PAD.encode(edge_signature.to_bytes()),
    }))
}

async fn fetch_registration_challenge(
    http: &HttpClient,
    config: &EdgeConfig,
) -> anyhow::Result<RegistrationChallengeResponse> {
    let response = http
        .get(format!(
            "{}/api/v1/trust/registration-challenge",
            config.api_url
        ))
        .send()
        .await
        .context("failed to request registration challenge")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("registration challenge request failed with status {status}: {body}");
    }

    response
        .json::<RegistrationChallengeResponse>()
        .await
        .context("failed to decode registration challenge response")
}

fn decode_signing_key(value: &str, label: &str) -> anyhow::Result<SigningKey> {
    let bytes = decode_base64_bytes(value, label)?;
    let key_bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .with_context(|| format!("{label} must decode to a 32-byte Ed25519 private key"))?;

    Ok(SigningKey::from_bytes(&key_bytes))
}

fn decode_verifying_key(value: &str, label: &str) -> anyhow::Result<VerifyingKey> {
    let bytes = decode_base64_bytes(value, label)?;
    let key_bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .with_context(|| format!("{label} must decode to a 32-byte Ed25519 public key"))?;

    VerifyingKey::from_bytes(&key_bytes)
        .with_context(|| format!("{label} is not a valid Ed25519 key"))
}

fn decode_signature(value: &str, label: &str) -> anyhow::Result<Signature> {
    let bytes = decode_base64_bytes(value, label)?;
    Signature::from_slice(&bytes)
        .with_context(|| format!("{label} must decode to a 64-byte Ed25519 signature"))
}

fn decode_base64_bytes(value: &str, label: &str) -> anyhow::Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value.trim())
        .with_context(|| format!("{label} is not valid base64url"))
}

fn orchestrator_challenge_payload(
    challenge_id: &str,
    challenge: &str,
    issued_at: DateTime<Utc>,
) -> String {
    format!(
        "elowen-orchestrator-registration-challenge\n{challenge_id}\n{challenge}\n{}",
        issued_at.to_rfc3339()
    )
}

fn edge_registration_payload(
    device_id: &str,
    name: &str,
    primary_flag: bool,
    challenge_id: &str,
    challenge: &str,
    challenge_issued_at: DateTime<Utc>,
    edge_public_key: &str,
) -> String {
    format!(
        "elowen-edge-registration\n{device_id}\n{name}\n{primary_flag}\n{challenge_id}\n{challenge}\n{}\n{edge_public_key}",
        challenge_issued_at.to_rfc3339()
    )
}

pub(crate) fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
