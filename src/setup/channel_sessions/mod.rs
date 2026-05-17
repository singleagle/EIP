//! Host-managed setup sessions for extension-declared channel setup flows.
//!
//! This mirrors the setup-tunnel shape: core code owns the lifecycle and
//! persistence, while provider modules own protocol details.

use std::collections::HashMap;
use std::time::SystemTime;

use crate::channels::wasm::{SetupSessionProvider, SetupSessionSchema};
use crate::extensions::{ExtensionError, SetupSessionResult, SetupSessionState};

mod ilink_qr;

#[cfg(test)]
pub(crate) use ilink_qr::{ilink_qr_setup_state, ilink_qr_setup_url, normalize_setup_base_url};

#[derive(Debug, Clone)]
pub(crate) struct PendingSetupSession {
    pub user_id: String,
    pub extension_name: String,
    pub expires_at: SystemTime,
    provider: PendingSetupProvider,
}

#[derive(Debug, Clone)]
enum PendingSetupProvider {
    IlinkQr(ilink_qr::IlinkQrSession),
}

#[derive(Debug, Clone)]
pub(crate) struct SetupSessionSecret {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SetupSessionCompletion {
    pub secrets: Vec<SetupSessionSecret>,
    pub runtime_config: HashMap<String, serde_json::Value>,
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug)]
pub(crate) struct SetupSessionPoll {
    pub result: SetupSessionResult,
    pub completion: Option<SetupSessionCompletion>,
    pub remove_session: bool,
}

pub(crate) fn format_system_time(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339()
}

pub(crate) fn setup_session_runtime_config_setting_key(name: &str, config_key: &str) -> String {
    format!("extensions.{name}.runtime_config.{config_key}")
}

pub(crate) fn setup_session_metadata_setting_key(name: &str, metadata_key: &str) -> String {
    format!("extensions.{name}.setup_session.{metadata_key}")
}

pub(crate) fn required_secret_names(
    schema: &SetupSessionSchema,
) -> Result<Vec<String>, ExtensionError> {
    match schema.provider {
        SetupSessionProvider::IlinkQr => ilink_qr::required_secret_names(schema),
    }
}

pub(crate) fn runtime_config_keys(
    schema: &SetupSessionSchema,
) -> Result<Vec<String>, ExtensionError> {
    match schema.provider {
        SetupSessionProvider::IlinkQr => ilink_qr::runtime_config_keys(schema),
    }
}

pub(crate) async fn begin_setup_session(
    extension_name: &str,
    user_id: &str,
    channel_config: &HashMap<String, serde_json::Value>,
    schema: &SetupSessionSchema,
) -> Result<(String, PendingSetupSession, SetupSessionResult), ExtensionError> {
    match schema.provider {
        SetupSessionProvider::IlinkQr => {
            let session_id = uuid::Uuid::new_v4().to_string();
            let (provider_session, result) =
                ilink_qr::begin(extension_name, &session_id, channel_config, schema).await?;
            let pending = PendingSetupSession {
                user_id: user_id.to_string(),
                extension_name: extension_name.to_string(),
                expires_at: provider_session.expires_at,
                provider: PendingSetupProvider::IlinkQr(provider_session),
            };
            Ok((session_id, pending, result))
        }
    }
}

pub(crate) async fn poll_setup_session(
    session_id: &str,
    session: &PendingSetupSession,
) -> Result<SetupSessionPoll, ExtensionError> {
    if SystemTime::now() >= session.expires_at {
        return Ok(SetupSessionPoll {
            result: SetupSessionResult {
                session_id: session_id.to_string(),
                extension_name: session.extension_name.clone(),
                state: SetupSessionState::Expired,
                qr_image_url: None,
                expires_at: Some(format_system_time(session.expires_at)),
                message: Some("QR code expired. Start a new setup session.".to_string()),
                account_id: None,
            },
            completion: None,
            remove_session: true,
        });
    }

    match &session.provider {
        PendingSetupProvider::IlinkQr(provider_session) => {
            ilink_qr::poll(session_id, &session.extension_name, provider_session).await
        }
    }
}

pub(crate) fn cancelled_result(
    session_id: &str,
    session: &PendingSetupSession,
) -> SetupSessionResult {
    SetupSessionResult {
        session_id: session_id.to_string(),
        extension_name: session.extension_name.clone(),
        state: SetupSessionState::Cancelled,
        qr_image_url: None,
        expires_at: Some(format_system_time(session.expires_at)),
        message: Some("Setup session cancelled.".to_string()),
        account_id: None,
    }
}
