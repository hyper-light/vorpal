//! The multi-project registry (D4): named `(src, index)` roots one daemon may serve.
//!
//! Enrollment is HUMAN-ONLY, by design and by construction: the registry file is written
//! exclusively by the `vorpal mcp allow`/`deny` CLI commands a person types; nothing reachable
//! through the MCP surface can add, change, or remove a root. The reasoning (adopted from the
//! comparison target's allow-root threat model): a confirmation delivered through the MCP
//! surface would be answered by the same agent that may have been influenced — so the surface
//! never gets the question. The serving side only ever LOADS the file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectEntry {
  /// Canonicalized source root.
  pub src: PathBuf,
  /// Index root served for this project (default: `<src>/.vorpal/index`).
  pub index: PathBuf,
}

/// `name → entry`, ordered — listings and defaults are deterministic.
pub type Projects = BTreeMap<String, ProjectEntry>;

/// The registry path: `$VORPAL_PROJECTS_FILE` override (tests, unusual setups), else
/// `~/.config/vorpal/projects.yml`.
pub fn registry_path() -> Option<PathBuf> {
  if let Some(explicit) = std::env::var_os("VORPAL_PROJECTS_FILE") {
    return Some(PathBuf::from(explicit));
  }
  let home = std::env::var_os("HOME")?;
  Some(
    PathBuf::from(home)
      .join(".config")
      .join("vorpal")
      .join("projects.yml"),
  )
}

pub fn load() -> Result<Projects, String> {
  let Some(path) = registry_path() else {
    return Err("cannot locate the projects registry (no HOME)".to_string());
  };
  if !path.exists() {
    return Ok(Projects::new());
  }
  let text =
    fs::read_to_string(&path).map_err(|err| format!("reading {}: {err}", path.display()))?;
  serde_yaml::from_str(&text).map_err(|err| format!("parsing {}: {err}", path.display()))
}

fn save(projects: &Projects) -> Result<PathBuf, String> {
  let Some(path) = registry_path() else {
    return Err("cannot locate the projects registry (no HOME)".to_string());
  };
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).map_err(|err| format!("creating {}: {err}", parent.display()))?;
  }
  let text = serde_yaml::to_string(projects).map_err(|err| format!("serializing registry: {err}"))?;
  // tmp + rename: a crash mid-write can never truncate the enrollment list.
  let tmp = path.with_extension("yml.tmp");
  fs::write(&tmp, text).map_err(|err| format!("writing {}: {err}", tmp.display()))?;
  fs::rename(&tmp, &path).map_err(|err| format!("installing {}: {err}", path.display()))?;
  Ok(path)
}

/// Enroll `src` under `name` (default: the directory's file name). Re-enrolling the same name
/// with the same src updates the index path; the same name with a DIFFERENT src is refused —
/// silently retargeting an enrolled name is exactly the substitution the human gate exists to
/// prevent.
pub fn enroll(
  src: &Path,
  name: Option<&str>,
  index: Option<&Path>,
) -> Result<(String, ProjectEntry, PathBuf), String> {
  let src = src
    .canonicalize()
    .map_err(|err| format!("source {} is not usable: {err}", src.display()))?;
  if !src.is_dir() {
    return Err(format!("source {} is not a directory", src.display()));
  }
  let name = match name {
    Some(explicit) => explicit.to_string(),
    None => src
      .file_name()
      .map(|n| n.to_string_lossy().into_owned())
      .ok_or_else(|| format!("cannot derive a project name from {}", src.display()))?,
  };
  let entry = ProjectEntry {
    index: index
      .map(Path::to_path_buf)
      .unwrap_or_else(|| src.join(".vorpal").join("index")),
    src,
  };
  let mut projects = load()?;
  if let Some(existing) = projects.get(&name) {
    if existing.src != entry.src {
      return Err(format!(
        "project '{name}' is already enrolled for {} — refusing to retarget it to {}; \
         `vorpal mcp deny {name}` first if that is really intended",
        existing.src.display(),
        entry.src.display()
      ));
    }
  }
  projects.insert(name.clone(), entry.clone());
  let path = save(&projects)?;
  Ok((name, entry, path))
}

pub fn remove(name: &str) -> Result<PathBuf, String> {
  let mut projects = load()?;
  if projects.remove(name).is_none() {
    return Err(format!("no project named '{name}' is enrolled"));
  }
  save(&projects)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn enroll_list_refuse_retarget_remove() {
    let base = std::env::temp_dir().join(format!("vorpal-registry-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let a = base.join("alpha");
    let b = base.join("beta");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    // SAFETY: test-scoped env var; the suite runs registry tests in this one process section.
    unsafe { std::env::set_var("VORPAL_PROJECTS_FILE", base.join("projects.yml")) };

    let (name, entry, _) = enroll(&a, None, None).expect("enroll alpha");
    assert_eq!(name, "alpha");
    assert!(entry.index.ends_with(".vorpal/index"));
    enroll(&b, Some("beta"), None).expect("enroll beta");
    assert_eq!(load().unwrap().len(), 2);

    // Same name, same src: updates. Same name, different src: refused.
    enroll(&a, Some("alpha"), Some(&base.join("elsewhere-index"))).expect("re-enroll updates");
    let err = enroll(&b, Some("alpha"), None).expect_err("retarget refused");
    assert!(err.contains("refusing to retarget"), "{err}");

    remove("beta").expect("remove");
    assert_eq!(load().unwrap().len(), 1);
    assert!(remove("beta").is_err(), "double-remove is an error");

    unsafe { std::env::remove_var("VORPAL_PROJECTS_FILE") };
    let _ = fs::remove_dir_all(&base);
  }
}
