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

#[derive(Deserialize, Serialize)]
struct Preferences {
    model: String,
    #[serde(default)]
    approvals_reviewer: Reviewer,
}

pub fn select_model(path: &Path, explicit: Option<&str>) -> Result<String> {
    let model = if let Some(model) = explicit {
        model.to_owned()
    } else {
        let bytes = std::fs::read(path).with_context(|| {
            format!(
                "read {}; choose --model or set OPENAI_MODEL; run qaf models for choices",
                path.display()
            )
        })?;
        serde_json::from_slice::<Preferences>(&bytes)
            .context("read saved model preference; use --model to replace it")?
            .model
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
    preferences.insert("model".into(), serde_json::json!(model));
    serde_json::to_writer(&mut file, &preferences)?;
    file.flush()?;
    file.persist(path).context("save model preference")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_selection_preserves_review_settings_and_rejects_corrupt_saves() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("qaf/preferences.json");
        assert!(select_model(&path, None).is_err());
        assert_eq!(reviewer(&path).unwrap(), Reviewer::User);
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
            r#"{"model":"first","approvals_reviewer":"auto_review"}"#,
        )
        .unwrap();
        remember_model(&path, "second").unwrap();
        assert_eq!(reviewer(&path).unwrap(), Reviewer::AutoReview);
        assert_eq!(select_model(&path, None).unwrap(), "second");
        assert!(select_model(&path, Some("")).is_err());
        std::fs::write(&path, r#"{"model":"second","approvals_reviewer":"typo"}"#).unwrap();
        assert!(reviewer(&path).is_err());
    }
}
