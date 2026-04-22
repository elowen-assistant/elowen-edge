//! Device registration and trust proof helpers.

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use reqwest::Client as HttpClient;

use crate::{
    config::{EdgeConfig, TrustedOrchestratorKey},
    contracts::{
        DeviceRegistrationTrustProof, RegisterDeviceRequest, RegistrationChallengeResponse,
        RegistrationTrustIntent,
    },
    discovery::{discover_repositories, discover_repository_catalog},
};

/// Generates and prints a new edge signing keypair for operator setup.
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

/// Retries initial device registration until the edge is trusted and visible.
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

/// Publishes the current device registration payload to the orchestrator API.
pub(crate) async fn register_device(http: &HttpClient, config: &EdgeConfig) -> anyhow::Result<()> {
    let discovered_repos =
        discover_repositories(&config.allowed_repo_roots, &config.excluded_repo_paths)
            .context("failed to discover repositories from configured roots")?;
    let repositories =
        discover_repository_catalog(&config.allowed_repo_roots, &config.excluded_repo_paths)
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
    if config.trusted_orchestrator_keys.is_empty() {
        return Ok(None);
    }
    let Some(edge_signing_key) = config.edge_signing_key.as_deref() else {
        anyhow::bail!(
            "ELOWEN_EDGE_SIGNING_KEY is required when trusted orchestrator key pinning is configured"
        );
    };

    let trusted_orchestrator_keys =
        parse_trusted_orchestrator_keys(&config.trusted_orchestrator_keys)?;
    let edge_signing_key = decode_signing_key(edge_signing_key, "edge signing key")?;
    let previous_edge_signing_key = config
        .previous_edge_signing_key
        .as_deref()
        .map(|value| decode_signing_key(value, "previous edge signing key"))
        .transpose()?;
    let challenge = fetch_registration_challenge(http, config).await?;
    let orchestrator_key = select_trusted_orchestrator_key(&trusted_orchestrator_keys, &challenge)?;

    let challenge_signature =
        decode_signature(&challenge.signature, "orchestrator challenge signature")?;
    let challenge_payload = orchestrator_challenge_payload(
        &challenge.challenge_id,
        &challenge.challenge,
        challenge.issued_at,
    );
    orchestrator_key
        .verifying_key
        .verify(challenge_payload.as_bytes(), &challenge_signature)
        .context("failed to verify orchestrator registration challenge signature")?;

    let edge_public_key = URL_SAFE_NO_PAD.encode(edge_signing_key.verifying_key().to_bytes());
    let registration_payload = edge_registration_payload(EdgeRegistrationPayloadInput {
        device_id: &config.device_id,
        name: &config.device_name,
        primary_flag: config.primary_flag,
        challenge_id: &challenge.challenge_id,
        challenge: &challenge.challenge,
        challenge_issued_at: challenge.issued_at,
        orchestrator_key_id: challenge
            .orchestrator_key_id
            .as_deref()
            .or(orchestrator_key.key_id.as_deref())
            .unwrap_or(challenge.orchestrator_public_key.as_str()),
        orchestrator_public_key: &challenge.orchestrator_public_key,
        edge_public_key: &edge_public_key,
    });
    let edge_signature = edge_signing_key.sign(registration_payload.as_bytes());
    let trusted_orchestrator_public_keys = trusted_orchestrator_keys
        .iter()
        .map(|entry| entry.public_key.clone())
        .collect::<Vec<_>>();
    let trusted_orchestrator_key_ids = trusted_orchestrator_keys
        .iter()
        .filter_map(|entry| entry.key_id.clone())
        .collect::<Vec<_>>();
    let reenrollment = previous_edge_signing_key.as_ref().map(|previous_key| {
        let previous_edge_public_key =
            URL_SAFE_NO_PAD.encode(previous_key.verifying_key().to_bytes());
        let previous_registration_payload =
            edge_registration_payload(EdgeRegistrationPayloadInput {
                device_id: &config.device_id,
                name: &config.device_name,
                primary_flag: config.primary_flag,
                challenge_id: &challenge.challenge_id,
                challenge: &challenge.challenge,
                challenge_issued_at: challenge.issued_at,
                orchestrator_key_id: challenge
                    .orchestrator_key_id
                    .as_deref()
                    .or(orchestrator_key.key_id.as_deref())
                    .unwrap_or(challenge.orchestrator_public_key.as_str()),
                orchestrator_public_key: &challenge.orchestrator_public_key,
                edge_public_key: &previous_edge_public_key,
            });
        let previous_edge_signature = previous_key.sign(previous_registration_payload.as_bytes());

        (
            previous_edge_public_key,
            URL_SAFE_NO_PAD.encode(previous_edge_signature.to_bytes()),
        )
    });

    Ok(Some(DeviceRegistrationTrustProof {
        trusted_orchestrator_public_keys,
        trusted_orchestrator_key_ids: (!trusted_orchestrator_key_ids.is_empty())
            .then_some(trusted_orchestrator_key_ids),
        orchestrator_key_id: challenge
            .orchestrator_key_id
            .clone()
            .or(orchestrator_key.key_id.clone())
            .unwrap_or_else(|| challenge.orchestrator_public_key.clone()),
        orchestrator_challenge_id: challenge.challenge_id,
        orchestrator_challenge: challenge.challenge,
        orchestrator_challenge_issued_at: challenge.issued_at,
        orchestrator_public_key: challenge.orchestrator_public_key,
        orchestrator_signature: challenge.signature,
        edge_public_key,
        edge_signature: URL_SAFE_NO_PAD.encode(edge_signature.to_bytes()),
        registration_intent: if previous_edge_signing_key.is_some() {
            RegistrationTrustIntent::Rotate
        } else {
            RegistrationTrustIntent::Enroll
        },
        previous_edge_public_key: reenrollment
            .as_ref()
            .map(|(public_key, _)| public_key.clone()),
        previous_edge_signature: reenrollment.map(|(_, signature)| signature),
        reenrollment_kind: previous_edge_signing_key
            .as_ref()
            .map(|_| "replace_existing_device_key".to_string()),
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

fn parse_trusted_orchestrator_keys(
    configured_keys: &[TrustedOrchestratorKey],
) -> anyhow::Result<Vec<TrustedOrchestratorKeyMaterial>> {
    configured_keys
        .iter()
        .map(|entry| {
            Ok(TrustedOrchestratorKeyMaterial {
                key_id: entry.key_id.clone(),
                public_key: entry.public_key.trim().to_string(),
                verifying_key: decode_verifying_key(&entry.public_key, "orchestrator public key")?,
            })
        })
        .collect()
}

fn select_trusted_orchestrator_key<'a>(
    trusted_keys: &'a [TrustedOrchestratorKeyMaterial],
    challenge: &RegistrationChallengeResponse,
) -> anyhow::Result<&'a TrustedOrchestratorKeyMaterial> {
    let challenge_public_key = challenge.orchestrator_public_key.trim();
    let challenge_key_id = challenge
        .orchestrator_key_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut matches = trusted_keys.iter().filter(|entry| {
        if entry.public_key != challenge_public_key {
            return false;
        }

        match challenge_key_id {
            Some(expected_key_id) => entry
                .key_id
                .as_deref()
                .map(|configured_key_id| configured_key_id == expected_key_id)
                .unwrap_or(true),
            None => true,
        }
    });

    let Some(selected) = matches.next() else {
        anyhow::bail!("registration challenge was signed by an unexpected orchestrator public key");
    };

    if matches.next().is_some() {
        anyhow::bail!(
            "registration challenge matched multiple trusted orchestrator keys; fix duplicate configuration"
        );
    }

    Ok(selected)
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

struct EdgeRegistrationPayloadInput<'a> {
    device_id: &'a str,
    name: &'a str,
    primary_flag: bool,
    challenge_id: &'a str,
    challenge: &'a str,
    challenge_issued_at: DateTime<Utc>,
    orchestrator_key_id: &'a str,
    orchestrator_public_key: &'a str,
    edge_public_key: &'a str,
}

fn edge_registration_payload(payload: EdgeRegistrationPayloadInput<'_>) -> String {
    format!(
        "elowen-edge-registration\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        payload.device_id,
        payload.name,
        payload.primary_flag,
        payload.challenge_id,
        payload.challenge,
        payload.challenge_issued_at.to_rfc3339(),
        payload.orchestrator_key_id,
        payload.orchestrator_public_key,
        payload.edge_public_key
    )
}

pub(crate) fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug)]
struct TrustedOrchestratorKeyMaterial {
    key_id: Option<String>,
    public_key: String,
    verifying_key: VerifyingKey,
}

#[cfg(test)]
mod tests {
    use super::{
        EdgeRegistrationPayloadInput, decode_signing_key, edge_registration_payload,
        orchestrator_challenge_payload, parse_trusted_orchestrator_keys,
        select_trusted_orchestrator_key,
    };
    use crate::{
        config::TrustedOrchestratorKey,
        contracts::{
            DeviceRegistrationTrustProof, RegistrationChallengeResponse, RegistrationTrustIntent,
        },
    };
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use chrono::Utc;
    use ed25519_dalek::{Signer, SigningKey, Verifier};

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn selects_challenge_key_by_key_id_when_present() {
        let current = signing_key(7);
        let next = signing_key(8);
        let trusted = parse_trusted_orchestrator_keys(&[
            TrustedOrchestratorKey {
                key_id: Some("current".to_string()),
                public_key: URL_SAFE_NO_PAD.encode(current.verifying_key().to_bytes()),
            },
            TrustedOrchestratorKey {
                key_id: Some("next".to_string()),
                public_key: URL_SAFE_NO_PAD.encode(next.verifying_key().to_bytes()),
            },
        ])
        .unwrap();
        let challenge = RegistrationChallengeResponse {
            challenge_id: "challenge-1".to_string(),
            challenge: "token".to_string(),
            issued_at: Utc::now(),
            orchestrator_key_id: Some("next".to_string()),
            orchestrator_public_key: URL_SAFE_NO_PAD.encode(next.verifying_key().to_bytes()),
            trusted_signers: Vec::new(),
            signature: "signature".to_string(),
        };

        let selected = select_trusted_orchestrator_key(&trusted, &challenge).unwrap();

        assert_eq!(selected.key_id.as_deref(), Some("next"));
        assert_eq!(
            selected.public_key,
            URL_SAFE_NO_PAD.encode(next.verifying_key().to_bytes())
        );
    }

    #[test]
    fn rejects_unknown_challenge_key_even_during_rotation() {
        let current = signing_key(9);
        let trusted = parse_trusted_orchestrator_keys(&[TrustedOrchestratorKey {
            key_id: Some("current".to_string()),
            public_key: URL_SAFE_NO_PAD.encode(current.verifying_key().to_bytes()),
        }])
        .unwrap();
        let unknown = signing_key(10);
        let challenge = RegistrationChallengeResponse {
            challenge_id: "challenge-2".to_string(),
            challenge: "token".to_string(),
            issued_at: Utc::now(),
            orchestrator_key_id: Some("next".to_string()),
            orchestrator_public_key: URL_SAFE_NO_PAD.encode(unknown.verifying_key().to_bytes()),
            trusted_signers: Vec::new(),
            signature: "signature".to_string(),
        };

        let error = select_trusted_orchestrator_key(&trusted, &challenge).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unexpected orchestrator public key")
        );
    }

    #[test]
    fn accepts_public_key_only_pin_when_challenge_includes_key_id() {
        let current = signing_key(10);
        let trusted = parse_trusted_orchestrator_keys(&[TrustedOrchestratorKey {
            key_id: None,
            public_key: URL_SAFE_NO_PAD.encode(current.verifying_key().to_bytes()),
        }])
        .unwrap();
        let challenge = RegistrationChallengeResponse {
            challenge_id: "challenge-compat".to_string(),
            challenge: "token".to_string(),
            issued_at: Utc::now(),
            orchestrator_key_id: Some("orchestrator-1-current".to_string()),
            orchestrator_public_key: URL_SAFE_NO_PAD.encode(current.verifying_key().to_bytes()),
            trusted_signers: Vec::new(),
            signature: "signature".to_string(),
        };

        let selected = select_trusted_orchestrator_key(&trusted, &challenge).unwrap();

        assert_eq!(selected.key_id, None);
        assert_eq!(
            selected.public_key,
            URL_SAFE_NO_PAD.encode(current.verifying_key().to_bytes())
        );
    }

    #[test]
    fn reenrollment_proof_can_bind_previous_and_current_edge_keys() {
        let current = signing_key(11);
        let previous = signing_key(12);
        let issued_at = Utc::now();
        let challenge_id = "challenge-3";
        let challenge = "token";
        let current_public_key = URL_SAFE_NO_PAD.encode(current.verifying_key().to_bytes());
        let previous_public_key = URL_SAFE_NO_PAD.encode(previous.verifying_key().to_bytes());
        let current_payload = edge_registration_payload(EdgeRegistrationPayloadInput {
            device_id: "device-1",
            name: "Device One",
            primary_flag: true,
            challenge_id,
            challenge,
            challenge_issued_at: issued_at,
            orchestrator_key_id: "orchestrator-1-current",
            orchestrator_public_key: "pinned-key",
            edge_public_key: &current_public_key,
        });
        let previous_payload = edge_registration_payload(EdgeRegistrationPayloadInput {
            device_id: "device-1",
            name: "Device One",
            primary_flag: true,
            challenge_id,
            challenge,
            challenge_issued_at: issued_at,
            orchestrator_key_id: "orchestrator-1-current",
            orchestrator_public_key: "pinned-key",
            edge_public_key: &previous_public_key,
        });
        let proof = DeviceRegistrationTrustProof {
            trusted_orchestrator_public_keys: vec!["pinned-key".to_string()],
            trusted_orchestrator_key_ids: Some(vec!["current".to_string(), "next".to_string()]),
            orchestrator_key_id: "next".to_string(),
            orchestrator_challenge_id: challenge_id.to_string(),
            orchestrator_challenge: challenge.to_string(),
            orchestrator_challenge_issued_at: issued_at,
            orchestrator_public_key: "pinned-key".to_string(),
            orchestrator_signature: "signature".to_string(),
            edge_public_key: current_public_key.clone(),
            edge_signature: URL_SAFE_NO_PAD
                .encode(current.sign(current_payload.as_bytes()).to_bytes()),
            registration_intent: RegistrationTrustIntent::Rotate,
            previous_edge_public_key: Some(previous_public_key.clone()),
            previous_edge_signature: Some(
                URL_SAFE_NO_PAD.encode(previous.sign(previous_payload.as_bytes()).to_bytes()),
            ),
            reenrollment_kind: Some("replace_existing_device_key".to_string()),
        };

        let current_signature =
            super::decode_signature(&proof.edge_signature, "edge signature").unwrap();
        current
            .verifying_key()
            .verify(current_payload.as_bytes(), &current_signature)
            .unwrap();

        let previous_signature = super::decode_signature(
            proof.previous_edge_signature.as_deref().unwrap(),
            "previous edge signature",
        )
        .unwrap();
        previous
            .verifying_key()
            .verify(previous_payload.as_bytes(), &previous_signature)
            .unwrap();
    }

    #[test]
    fn challenge_payload_format_stays_stable() {
        let issued_at = Utc::now();
        let payload = orchestrator_challenge_payload("challenge-4", "token", issued_at);
        assert!(payload.contains("elowen-orchestrator-registration-challenge"));
        assert!(payload.contains("challenge-4"));
    }

    #[test]
    fn signing_key_decoder_requires_32_bytes() {
        let error = decode_signing_key("abc", "edge signing key").unwrap_err();
        assert!(error.to_string().contains("32-byte Ed25519 private key"));
    }
}
