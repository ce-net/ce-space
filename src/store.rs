//! On-disk persistence: one JSON snapshot of the SpaceMachine, written
//! atomically (temp + rename). The machine's op/LWW design is what makes a
//! future replicated log (ce-coord) a drop-in upgrade; v1 persists state on
//! the owning node only.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::space::SpaceMachine;

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> Store {
        Store { path: path.to_path_buf() }
    }

    pub fn load(&self) -> Result<SpaceMachine> {
        if !self.path.exists() {
            return Ok(SpaceMachine::default());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", self.path.display()))
    }

    pub fn save(&self, m: &SpaceMachine) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(m)?;
        fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming into {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{Op, Value};

    #[test]
    fn roundtrip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grid.json");
        let store = Store::open(&path);
        // Missing file loads as empty machine.
        assert!(store.load().unwrap().spaces.is_empty());

        let mut m = SpaceMachine::default();
        let op = m.stamp("n", Op::CreateSpace { space: "s".into() });
        m.apply(&op);
        let op = m.stamp("n", Op::SetCell {
            space: "s".into(),
            coords: [("k".to_string(), "x".to_string())].into_iter().collect(),
            value: Value::Text { text: "v".into() },
        });
        m.apply(&op);
        store.save(&m).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded, m);
        // No stray temp file.
        assert!(!path.with_extension("json.tmp").exists());
    }
}
