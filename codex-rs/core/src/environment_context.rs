use serde::Deserialize;
use serde::Serialize;
use strum_macros::Display as DeriveDisplay;

use crate::codex::Session;
use crate::config_types::SandboxMode;
use crate::models::ContentItem;
use crate::models::ResponseItem;
use crate::protocol::AskForApproval;
use crate::protocol::SandboxPolicy;
use std::fmt::Display;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, DeriveDisplay)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "snake_case")]
pub enum NetworkAccess {
    Restricted,
    Enabled,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename = "environment_context", rename_all = "snake_case")]
pub(crate) struct EnvironmentContext {
    pub cwd: PathBuf,
    pub approval_policy: AskForApproval,
    pub sandbox_mode: SandboxMode,
    pub network_access: NetworkAccess,
}

impl Display for EnvironmentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "Current working directory: {}",
            self.cwd.to_string_lossy()
        )?;
        writeln!(f, "Approval policy: {}", self.approval_policy)?;
        writeln!(f, "Sandbox mode: {}", self.sandbox_mode)?;
        writeln!(f, "Network access: {}", self.network_access)?;
        Ok(())
    }
}

impl From<&Session> for EnvironmentContext {
    fn from(sess: &Session) -> Self {
        EnvironmentContext {
            cwd: sess.cwd.clone(),
            approval_policy: sess.approval_policy,
            sandbox_mode: match sess.sandbox_policy.clone() {
                SandboxPolicy::DangerFullAccess => SandboxMode::DangerFullAccess,
                SandboxPolicy::ReadOnly => SandboxMode::ReadOnly,
                SandboxPolicy::WorkspaceWrite { .. } => SandboxMode::WorkspaceWrite,
            },
            network_access: match sess.sandbox_policy.clone() {
                SandboxPolicy::DangerFullAccess => NetworkAccess::Enabled,
                SandboxPolicy::ReadOnly => NetworkAccess::Restricted,
                SandboxPolicy::WorkspaceWrite { network_access, .. } => {
                    if network_access {
                        NetworkAccess::Enabled
                    } else {
                        NetworkAccess::Restricted
                    }
                }
            },
        }
    }
}

impl From<EnvironmentContext> for ResponseItem {
    fn from(ec: EnvironmentContext) -> Self {
        let mut buffer = String::new();
        let mut ser = quick_xml::se::Serializer::new(&mut buffer);
        ser.indent(' ', 2);

        let text = match ec.serialize(ser) {
            Ok(_) => buffer,
            Err(err) => {
                tracing::error!("Error serializing environment context: {err}");
                // if we can't serialize the environment context, fallback to the string representation
                ec.to_string()
            }
        };

        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text }],
        }
    }
}
