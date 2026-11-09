use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path};

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Reviewer {
    #[default]
    User,
    AutoReview,
}

pub fn reviewer(path: &Path) -> Result<Reviewer> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).context("read preferences")?;
            value
                .get("approvals_reviewer")
                .cloned()
                .map_or(Ok(Reviewer::User), |value| {
                    serde_json::from_value(value)
                        .context("approvals_reviewer must be user or auto_review")
                })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Reviewer::User),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub fast: bool,
}

pub fn model_settings(path: &Path, role: &str) -> Result<ModelSettings> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelSettings::default());
        }
        Err(error) => return Err(error.into()),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).context("read preferences")?;
    let legacy = if role == "model_primary" {
        "model"
    } else {
        "reviewer_model"
    };
    let settings: ModelSettings = if let Some(value) = value.get(role) {
        serde_json::from_value(value.clone()).with_context(|| format!("read {role}"))?
    } else {
        ModelSettings {
            name: value
                .get(legacy)
                .map(|v| serde_json::from_value::<Option<String>>(v.clone()))
                .transpose()?
                .flatten(),
            ..ModelSettings::default()
        }
    };
    if let Some(name) = &settings.name {
        ensure!(
            !name.is_empty() && !name.contains(char::is_whitespace),
            "{role}.name must be a nonempty model identifier"
        );
    }
    if let Some(effort) = &settings.reasoning_effort {
        ensure!(
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"
            ]
            .contains(&effort.as_str()),
            "invalid {role}.reasoning_effort"
        );
    }
    Ok(settings)
}

#[cfg(test)]
fn reviewer_model(path: &Path) -> Result<Option<String>> {
    Ok(model_settings(path, "model_reviewer")?.name)
}

pub fn select_model(path: &Path, explicit: Option<&str>) -> Result<String> {
    let model = if let Some(model) = explicit {
        model.to_owned()
    } else {
        model_settings(path, "model_primary")?
            .name
            .context("choose --model or set OPENAI_MODEL; run qaf models for choices")?
    };
    ensure!(!model.trim().is_empty(), "model must not be empty");
    Ok(model)
}

pub fn remember_model(path: &Path, model: &str) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    ensure!(!model.trim().is_empty(), "model must not be empty");
    let directory = path.parent().context("preferences path has no parent")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    // Preserve other settings when selecting a model. Malformed JSON is never overwritten.
    let mut preferences = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
            .context("read preferences before saving model")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(error.into()),
    };
    // Migrate legacy selections while retaining unknown top-level preferences.
    for (role, legacy) in [
        ("model_primary", "model"),
        ("model_reviewer", "reviewer_model"),
    ] {
        let settings = model_settings(path, role)?;
        if preferences.contains_key(role) || settings.name.is_some() {
            preferences.insert(role.into(), serde_json::to_value(settings)?);
        }
        preferences.remove(legacy);
    }
    let primary = preferences
        .entry("model_primary")
        .or_insert_with(|| serde_json::json!({}));
    primary
        .as_object_mut()
        .context("model_primary must be an object")?
        .insert("name".into(), serde_json::json!(model));
    serde_json::to_writer(&mut file, &preferences)?;
    file.flush()?;
    file.persist(path).context("save model preference")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_settings_survive_model_switches() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("preferences.json");
        let value = serde_json::json!({
            "model_primary": {"name":"first", "reasoning_effort":"medium", "fast":false},
            "model_reviewer": {"name":"reviewer", "fast":true},
            "custom": "preserved"
        });
        std::fs::write(&path, value.to_string()).unwrap();
        remember_model(&path, "second").unwrap();
        let primary = model_settings(&path, "model_primary").unwrap();
        let reviewer = model_settings(&path, "model_reviewer").unwrap();
        assert_eq!(primary.name.as_deref(), Some("second"));
        assert_eq!(primary.reasoning_effort.as_deref(), Some("medium"));
        assert!(!primary.fast);
        assert!(reviewer.fast);
        assert!(reviewer.reasoning_effort.is_none());
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["custom"], "preserved");
        assert!(saved.get("model").is_none());
    }

    #[test]
    fn model_selection_preserves_review_settings_and_rejects_corrupt_saves() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("qaf/preferences.json");
        assert!(select_model(&path, None).is_err());
        assert_eq!(reviewer(&path).unwrap(), Reviewer::User);
        assert_eq!(reviewer_model(&path).unwrap(), None);
        assert_eq!(select_model(&path, Some("first")).unwrap(), "first");
        remember_model(&path, "first").unwrap();
        assert_eq!(reviewer(&path).unwrap(), Reviewer::User);
        assert_eq!(select_model(&path, None).unwrap(), "first");
        assert!(remember_model(&path, "  ").is_err());
        assert_eq!(select_model(&path, None).unwrap(), "first");
        std::fs::write(&path, "broken json").unwrap();
        assert!(select_model(&path, None).is_err());
        assert_eq!(select_model(&path, Some("second")).unwrap(), "second");
        assert!(remember_model(&path, "second").is_err());
        std::fs::write(
            &path,
            r#"{"model":"first","approvals_reviewer":"auto_review","reviewer_model":"review-model"}"#,
        )
        .unwrap();
        remember_model(&path, "second").unwrap();
        assert_eq!(reviewer(&path).unwrap(), Reviewer::AutoReview);
        assert_eq!(
            reviewer_model(&path).unwrap().as_deref(),
            Some("review-model")
        );
        assert_eq!(select_model(&path, None).unwrap(), "second");
        assert!(select_model(&path, Some("")).is_err());
        std::fs::write(&path, r#"{"model":"second","approvals_reviewer":"typo"}"#).unwrap();
        assert!(reviewer(&path).is_err());
        for value in [
            serde_json::json!(""),
            serde_json::json!("two words"),
            serde_json::json!(42),
        ] {
            std::fs::write(
                &path,
                serde_json::json!({"reviewer_model":value}).to_string(),
            )
            .unwrap();
            assert!(reviewer_model(&path).is_err());
        }
        std::fs::write(&path, r#"{"reviewer_model":null}"#).unwrap();
        assert_eq!(reviewer_model(&path).unwrap(), None);
    }
}
