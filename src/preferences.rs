use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path};

#[derive(Deserialize, Serialize)]
struct Preferences {
    model: String,
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
    serde_json::to_writer(
        &mut file,
        &Preferences {
            model: model.into(),
        },
    )?;
    file.flush()?;
    file.persist(path).context("save model preference")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_model_and_explicit_selection_can_replace_bad_preferences() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("qaf/preferences.json");
        assert!(select_model(&path, None).is_err());
        assert_eq!(select_model(&path, Some("first")).unwrap(), "first");
        remember_model(&path, "first").unwrap();
        assert_eq!(select_model(&path, None).unwrap(), "first");
        assert!(remember_model(&path, "  ").is_err());
        assert_eq!(select_model(&path, None).unwrap(), "first");
        std::fs::write(&path, "broken json").unwrap();
        assert!(select_model(&path, None).is_err());
        assert_eq!(select_model(&path, Some("second")).unwrap(), "second");
        remember_model(&path, "second").unwrap();
        assert_eq!(select_model(&path, None).unwrap(), "second");
        assert!(select_model(&path, Some("")).is_err());
    }
}
