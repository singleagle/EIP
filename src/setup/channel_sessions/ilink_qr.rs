use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::channels::wasm::SetupSessionSchema;
use crate::extensions::{ExtensionError, SetupSessionResult, SetupSessionState};
use crate::setup::channel_sessions::{
    SetupSessionCompletion, SetupSessionPoll, SetupSessionSecret, format_system_time,
};

const DEFAULT_QR_SESSION_TTL_SECS: u64 = 8 * 60;

#[derive(Debug, Clone)]
pub(super) struct IlinkQrSession {
    pub expires_at: SystemTime,
    config: IlinkQrConfig,
    qrcode: String,
    qr_image_url: String,
    base_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct IlinkQrConfig {
    token_secret_name: String,
    #[serde(default)]
    base_url_config_key: Option<String>,
    #[serde(default)]
    bot_type_config_key: Option<String>,
    #[serde(default)]
    default_base_url: Option<String>,
    #[serde(default)]
    default_bot_type: Option<String>,
    #[serde(default)]
    runtime_config_writes: HashMap<String, String>,
    #[serde(default)]
    metadata_writes: HashMap<String, String>,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct IlinkQrCodeResponse {
    qrcode: String,
    qrcode_img_content: String,
}

#[derive(Debug, Deserialize)]
struct IlinkQrCodeStatusResponse {
    status: String,
    bot_token: Option<String>,
    ilink_bot_id: Option<String>,
    baseurl: Option<String>,
    ilink_user_id: Option<String>,
}

pub(super) fn required_secret_names(
    schema: &SetupSessionSchema,
) -> Result<Vec<String>, ExtensionError> {
    Ok(vec![parse_config(schema)?.token_secret_name])
}

pub(super) fn runtime_config_keys(
    schema: &SetupSessionSchema,
) -> Result<Vec<String>, ExtensionError> {
    Ok(parse_config(schema)?
        .runtime_config_writes
        .keys()
        .filter(|key| !key.trim().is_empty())
        .cloned()
        .collect())
}

pub(super) async fn begin(
    extension_name: &str,
    session_id: &str,
    channel_config: &HashMap<String, serde_json::Value>,
    schema: &SetupSessionSchema,
) -> Result<(IlinkQrSession, SetupSessionResult), ExtensionError> {
    let config = parse_config(schema)?;
    let base_url_key = config.base_url_config_key.as_deref().unwrap_or("base_url");
    let base_url = channel_config
        .get(base_url_key)
        .and_then(serde_json::Value::as_str)
        .or(config.default_base_url.as_deref())
        .ok_or_else(|| {
            ExtensionError::Other(format!(
                "Interactive setup for '{}' is missing a base URL",
                extension_name
            ))
        })?;
    let base_url = normalize_setup_base_url(base_url)?;
    let bot_type_key = config.bot_type_config_key.as_deref().unwrap_or("bot_type");
    let bot_type = channel_config
        .get(bot_type_key)
        .and_then(serde_json::Value::as_str)
        .or(config.default_bot_type.as_deref())
        .unwrap_or("3");
    let url = ilink_qr_setup_url(
        &base_url,
        &format!(
            "ilink/bot/get_bot_qrcode?bot_type={}",
            urlencoding::encode(bot_type)
        ),
    )?;

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| ExtensionError::Other(e.to_string()))?
        .get(url)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(
                is_timeout = e.is_timeout(),
                is_connect = e.is_connect(),
                status = e.status().map(|s| s.as_u16()),
                "iLink QR setup request failed"
            );
            ExtensionError::Other("iLink QR setup request failed".to_string())
        })?;

    if !response.status().is_success() {
        return Err(ExtensionError::Other(format!(
            "iLink QR setup request returned {}",
            response.status()
        )));
    }

    let qr: IlinkQrCodeResponse = response.json().await.map_err(|e| {
        tracing::warn!("Failed to parse iLink QR setup response: {}", e);
        ExtensionError::Other("Failed to parse iLink QR setup response".to_string())
    })?;
    if qr.qrcode.trim().is_empty() || qr.qrcode_img_content.trim().is_empty() {
        return Err(ExtensionError::Other(
            "iLink QR setup response did not include a QR code".to_string(),
        ));
    }

    let ttl_secs = config.ttl_secs.unwrap_or(DEFAULT_QR_SESSION_TTL_SECS);
    let expires_at = SystemTime::now() + Duration::from_secs(ttl_secs);
    let session = IlinkQrSession {
        expires_at,
        config,
        qrcode: qr.qrcode,
        qr_image_url: qr.qrcode_img_content.clone(),
        base_url,
    };
    let result = SetupSessionResult {
        session_id: session_id.to_string(),
        extension_name: extension_name.to_string(),
        state: SetupSessionState::QrRequired,
        qr_image_url: Some(qr.qrcode_img_content),
        expires_at: Some(format_system_time(expires_at)),
        message: Some("Scan this QR code to continue to connect the channel.".to_string()),
        account_id: None,
    };
    Ok((session, result))
}

pub(super) async fn poll(
    session_id: &str,
    extension_name: &str,
    session: &IlinkQrSession,
) -> Result<SetupSessionPoll, ExtensionError> {
    let url = ilink_qr_setup_url(
        &session.base_url,
        &format!(
            "ilink/bot/get_qrcode_status?qrcode={}",
            urlencoding::encode(&session.qrcode)
        ),
    )?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()
        .map_err(|e| ExtensionError::Other(e.to_string()))?
        .get(url)
        .header("iLink-App-ClientVersion", "1")
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(
                is_timeout = e.is_timeout(),
                is_connect = e.is_connect(),
                status = e.status().map(|s| s.as_u16()),
                "iLink QR status request failed"
            );
            ExtensionError::Other("iLink QR status request failed".to_string())
        })?;

    if !response.status().is_success() {
        return Err(ExtensionError::Other(format!(
            "iLink QR status request returned {}",
            response.status()
        )));
    }

    let status: IlinkQrCodeStatusResponse = response.json().await.map_err(|e| {
        tracing::warn!("Failed to parse iLink QR status response: {}", e);
        ExtensionError::Other("Failed to parse iLink QR status response".to_string())
    })?;
    let state = ilink_qr_setup_state(status.status.as_str());
    match state {
        SetupSessionState::WaitingScan | SetupSessionState::Scanned => Ok(SetupSessionPoll {
            result: SetupSessionResult {
                session_id: session_id.to_string(),
                extension_name: extension_name.to_string(),
                state,
                qr_image_url: Some(session.qr_image_url.clone()),
                expires_at: Some(format_system_time(session.expires_at)),
                message: Some(match state {
                    SetupSessionState::Scanned => {
                        "QR code scanned. Confirm login to continue.".to_string()
                    }
                    _ => "Waiting for QR scan.".to_string(),
                }),
                account_id: None,
            },
            completion: None,
            remove_session: false,
        }),
        SetupSessionState::Expired => Ok(SetupSessionPoll {
            result: SetupSessionResult {
                session_id: session_id.to_string(),
                extension_name: extension_name.to_string(),
                state,
                qr_image_url: None,
                expires_at: Some(format_system_time(session.expires_at)),
                message: Some("QR code expired. Start a new setup session.".to_string()),
                account_id: None,
            },
            completion: None,
            remove_session: true,
        }),
        SetupSessionState::Confirmed => {
            confirmed_result(session_id, extension_name, session, status)
        }
        _ => Ok(SetupSessionPoll {
            result: SetupSessionResult {
                session_id: session_id.to_string(),
                extension_name: extension_name.to_string(),
                state: SetupSessionState::Failed,
                qr_image_url: None,
                expires_at: Some(format_system_time(session.expires_at)),
                message: Some(format!("Unexpected iLink QR status: {}", status.status)),
                account_id: None,
            },
            completion: None,
            remove_session: true,
        }),
    }
}

fn confirmed_result(
    session_id: &str,
    extension_name: &str,
    session: &IlinkQrSession,
    status: IlinkQrCodeStatusResponse,
) -> Result<SetupSessionPoll, ExtensionError> {
    let bot_token = status
        .bot_token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            ExtensionError::Other(
                "iLink login confirmed but server did not return a bot token".to_string(),
            )
        })?;

    let mut completion = SetupSessionCompletion::default();
    completion.secrets.push(SetupSessionSecret {
        name: session.config.token_secret_name.clone(),
        value: bot_token.to_string(),
    });

    for (runtime_key, response_field) in &session.config.runtime_config_writes {
        if let Some(value) =
            status_response_field(&status, response_field).filter(|value| !value.trim().is_empty())
        {
            let value = if response_field == "baseurl" {
                normalize_setup_base_url(value)?
            } else {
                value.to_string()
            };
            completion
                .runtime_config
                .insert(runtime_key.clone(), serde_json::Value::String(value));
        }
    }

    for (metadata_key, response_field) in &session.config.metadata_writes {
        if let Some(value) =
            status_response_field(&status, response_field).filter(|value| !value.trim().is_empty())
        {
            completion.metadata.insert(
                metadata_key.clone(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    Ok(SetupSessionPoll {
        result: SetupSessionResult {
            session_id: session_id.to_string(),
            extension_name: extension_name.to_string(),
            state: SetupSessionState::Ready,
            qr_image_url: None,
            expires_at: None,
            message: Some("Channel connected.".to_string()),
            account_id: status.ilink_bot_id,
        },
        completion: Some(completion),
        remove_session: true,
    })
}

fn parse_config(schema: &SetupSessionSchema) -> Result<IlinkQrConfig, ExtensionError> {
    serde_json::from_value(schema.config.clone()).map_err(|e| {
        ExtensionError::Other(format!(
            "Invalid iLink QR setup session provider config: {e}"
        ))
    })
}

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

pub(crate) fn ilink_qr_setup_url(
    base_url: &str,
    path_and_query: &str,
) -> Result<String, ExtensionError> {
    let base = ensure_trailing_slash(base_url);
    let url = url::Url::parse(&format!("{base}{path_and_query}"))
        .map_err(|e| ExtensionError::Other(format!("Invalid setup URL: {e}")))?;
    crate::tools::builtin::skill_tools::validate_fetch_url(url.as_str())
        .map_err(|e| ExtensionError::Other(format!("SSRF blocked: {e}")))?;
    if url.scheme() != "https" {
        return Err(ExtensionError::Other(
            "setup URL must use HTTPS".to_string(),
        ));
    }
    if url.username() != "" || url.password().is_some() {
        return Err(ExtensionError::Other(
            "setup URL must not contain embedded credentials".to_string(),
        ));
    }
    Ok(url.to_string())
}

pub(crate) fn normalize_setup_base_url(base_url: &str) -> Result<String, ExtensionError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let parsed = url::Url::parse(trimmed)
        .map_err(|e| ExtensionError::Other(format!("Invalid setup base URL: {e}")))?;
    crate::tools::builtin::skill_tools::validate_fetch_url(parsed.as_str())
        .map_err(|e| ExtensionError::Other(format!("SSRF blocked: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(ExtensionError::Other(
            "setup base URL must use HTTPS".to_string(),
        ));
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return Err(ExtensionError::Other(
            "setup base URL must not contain embedded credentials".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn status_response_field<'a>(
    status: &'a IlinkQrCodeStatusResponse,
    field: &str,
) -> Option<&'a str> {
    match field {
        "ilink_bot_id" => status.ilink_bot_id.as_deref(),
        "ilink_user_id" => status.ilink_user_id.as_deref(),
        "baseurl" => status.baseurl.as_deref(),
        "bot_token" => status.bot_token.as_deref(),
        "status" => Some(status.status.as_str()),
        _ => None,
    }
}

pub(crate) fn ilink_qr_setup_state(status: &str) -> SetupSessionState {
    match status {
        "wait" => SetupSessionState::WaitingScan,
        "scaned" => SetupSessionState::Scanned,
        "confirmed" => SetupSessionState::Confirmed,
        "expired" => SetupSessionState::Expired,
        _ => SetupSessionState::Failed,
    }
}
