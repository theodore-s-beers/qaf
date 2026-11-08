use crate::tools::Tool;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, path::Path};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Sandbox,
    Unrestricted,
}

pub struct Request {
    pub tool: Tool,
    pub access: Access,
    pub justification: Option<String>,
}

impl Request {
    pub fn parse(name: &str, arguments: &str) -> Result<Self> {
        let mut arguments: Value =
            serde_json::from_str(arguments).context("invalid tool arguments")?;
        let object = arguments
            .as_object_mut()
            .context("tool arguments must be an object")?;
        let access = object
            .remove("access")
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let justification: Option<String> = object
            .remove("justification")
            .map(serde_json::from_value)
            .transpose()?
            .flatten();
        ensure!(
            access != Access::Unrestricted
                || justification.as_ref().is_some_and(|s| !s.trim().is_empty()),
            "unrestricted access requires a justification"
        );
        let tool = serde_json::from_value(json!({"name": name, "arguments": arguments}))
            .context("unknown tool or invalid arguments")?;
        Ok(Self {
            tool,
            access,
            justification,
        })
    }

    pub fn description(&self, directory: &Path) -> Value {
        json!({"operation": self.tool, "directory": directory, "access": self.access,
            "justification": self.justification})
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AllowOnce,
    Deny,
    Cancel,
}

pub trait Approver: Sync {
    fn approve(&self, request: &Value) -> impl Future<Output = Result<Decision>> + Send;
}

#[cfg(test)]
pub struct Deny;
#[cfg(test)]
impl Approver for Deny {
    fn approve(&self, _: &Value) -> impl Future<Output = Result<Decision>> + Send {
        std::future::ready(Ok(Decision::Deny))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broader_access_is_an_explicit_validated_request() {
        assert!(matches!(
            Request::parse("shell", r#"{"command":"true"}"#)
                .unwrap()
                .access,
            Access::Sandbox
        ));
        for arguments in [
            r#"{"command":"true","access":"unrestricted"}"#,
            r#"{"command":"true","access":"unrestricted","justification":""}"#,
            r#"{"command":"true","access":"approved"}"#,
            r#"{"command":"true","approved":true}"#,
        ] {
            assert!(Request::parse("shell", arguments).is_err());
        }
        assert!(
            Request::parse(
                "shell",
                r#"{"command":"true","access":"unrestricted","justification":"Needs network"}"#
            )
            .is_ok()
        );
    }
}
