//! `vorpal mcp install` (D5): write the MCP client configuration for the top clients —
//! idempotent JSON edits with a timestamped backup before any modification, `--dry-run` to
//! preview, and an explicit report of every file touched or skipped. Project-scoped files are
//! preferred wherever the client supports them (they travel with the repo and cannot clobber
//! a user's personal global config); global-only clients are edited in place with a backup.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Client {
  /// Claude Code — project-scoped `.mcp.json` (the standard shared-project location).
  ClaudeCode,
  /// Claude Desktop — global `claude_desktop_config.json`.
  ClaudeDesktop,
  /// Cursor — project-scoped `.cursor/mcp.json`.
  Cursor,
  /// VS Code — project-scoped `.vscode/mcp.json` (note: uses a `servers` key).
  Vscode,
  /// Windsurf — global `~/.codeium/windsurf/mcp_config.json`.
  Windsurf,
  /// Every client whose location can be determined on this machine.
  All,
}

struct Target {
  client: &'static str,
  path: PathBuf,
  /// Top-level key holding the server map (`mcpServers` everywhere except VS Code).
  key: &'static str,
  /// Global configs are only written when the client is plausibly present (its config dir
  /// exists); project configs are always writable.
  global: bool,
}

fn targets(client: Client, project_dir: &Path) -> Result<Vec<Target>> {
  let home = std::env::var_os("HOME")
    .map(PathBuf::from)
    .ok_or_else(|| anyhow!("cannot locate HOME"))?;
  let mut all = vec![
    Target {
      client: "claude-code",
      path: project_dir.join(".mcp.json"),
      key: "mcpServers",
      global: false,
    },
    Target {
      client: "claude-desktop",
      path: home
        .join("Library/Application Support/Claude")
        .join("claude_desktop_config.json"),
      key: "mcpServers",
      global: true,
    },
    Target {
      client: "cursor",
      path: project_dir.join(".cursor").join("mcp.json"),
      key: "mcpServers",
      global: false,
    },
    Target {
      client: "vscode",
      path: project_dir.join(".vscode").join("mcp.json"),
      key: "servers",
      global: false,
    },
    Target {
      client: "windsurf",
      path: home.join(".codeium/windsurf/mcp_config.json"),
      key: "mcpServers",
      global: true,
    },
  ];
  let wanted = |name: &str| match client {
    Client::All => true,
    Client::ClaudeCode => name == "claude-code",
    Client::ClaudeDesktop => name == "claude-desktop",
    Client::Cursor => name == "cursor",
    Client::Vscode => name == "vscode",
    Client::Windsurf => name == "windsurf",
  };
  all.retain(|t| wanted(t.client));
  Ok(all)
}

/// The server entry every client receives: the absolute path of THIS binary (survives
/// PATH-less launchers), serving the current project's index over stdio.
fn server_entry(command_override: Option<&str>) -> Result<Value> {
  let command = match command_override {
    Some(explicit) => explicit.to_string(),
    None => std::env::current_exe()
      .context("resolving the vorpal executable path")?
      .to_string_lossy()
      .into_owned(),
  };
  Ok(json!({"command": command, "args": ["mcp"]}))
}

pub fn run_install(
  client: Client,
  command_override: Option<&str>,
  dry_run: bool,
) -> Result<()> {
  let project_dir = std::env::current_dir().context("resolving the current directory")?;
  let entry = server_entry(command_override)?;
  let mut wrote = 0usize;
  for target in targets(client, &project_dir)? {
    // A global client we can't see on this machine is reported and skipped — installing
    // config for absent software is noise; project files are always fair game.
    if target.global && !target.path.parent().is_some_and(Path::exists) {
      println!(
        "skip  {:<14} {} (client not detected on this machine)",
        target.client,
        target.path.display()
      );
      continue;
    }
    let existing = match fs::read_to_string(&target.path) {
      Ok(text) => serde_json::from_str::<Value>(&text).with_context(|| {
        format!(
          "{} exists but is not valid JSON — fix or move it first (nothing was changed)",
          target.path.display()
        )
      })?,
      Err(_) => json!({}),
    };
    let mut updated = existing.clone();
    let root = updated
      .as_object_mut()
      .ok_or_else(|| anyhow!("{} is not a JSON object", target.path.display()))?;
    let servers = root
      .entry(target.key)
      .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers
      .as_object_mut()
      .ok_or_else(|| anyhow!("{}.{} is not an object", target.path.display(), target.key))?;
    if servers.get("vorpal") == Some(&entry) {
      println!(
        "ok    {:<14} {} (already configured)",
        target.client,
        target.path.display()
      );
      continue;
    }
    servers.insert("vorpal".to_string(), entry.clone());
    let rendered = format!("{}\n", serde_json::to_string_pretty(&updated)?);
    if dry_run {
      println!(
        "would {:<14} {}:\n{rendered}",
        target.client,
        target.path.display()
      );
      continue;
    }
    if let Some(parent) = target.path.parent() {
      fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    }
    if target.path.exists() {
      let backup = target.path.with_extension(format!(
        "json.bak-vorpal-{}",
        std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .map(|d| d.as_secs())
          .unwrap_or(0)
      ));
      fs::copy(&target.path, &backup)
        .with_context(|| format!("backing up {}", target.path.display()))?;
      println!("back  {:<14} {}", target.client, backup.display());
    }
    fs::write(&target.path, rendered)
      .with_context(|| format!("writing {}", target.path.display()))?;
    println!("wrote {:<14} {}", target.client, target.path.display());
    wrote += 1;
  }
  if dry_run {
    println!("(dry run — nothing was written)");
  } else if wrote == 0 {
    println!("nothing to do — every selected client is configured or not present");
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn install_is_idempotent_backs_up_and_respects_dry_run() {
    let base = std::env::temp_dir().join(format!("vorpal-install-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&base).unwrap();

    // Dry run writes nothing.
    run_install(Client::ClaudeCode, Some("/usr/local/bin/vorpal"), true).unwrap();
    assert!(!base.join(".mcp.json").exists());

    // First real run writes; second is a no-op (idempotent).
    run_install(Client::ClaudeCode, Some("/usr/local/bin/vorpal"), false).unwrap();
    let text = fs::read_to_string(base.join(".mcp.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["mcpServers"]["vorpal"]["command"], "/usr/local/bin/vorpal");
    run_install(Client::ClaudeCode, Some("/usr/local/bin/vorpal"), false).unwrap();
    let backups: Vec<_> = fs::read_dir(&base)
      .unwrap()
      .flatten()
      .filter(|e| e.file_name().to_string_lossy().contains("bak-vorpal"))
      .collect();
    assert!(backups.is_empty(), "idempotent re-run must not touch the file");

    // A foreign entry survives; ours is added beside it, with a backup taken.
    fs::write(
      base.join(".mcp.json"),
      r#"{"mcpServers": {"other": {"command": "x"}}}"#,
    )
    .unwrap();
    run_install(Client::ClaudeCode, Some("/usr/local/bin/vorpal"), false).unwrap();
    let v: Value =
      serde_json::from_str(&fs::read_to_string(base.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(v["mcpServers"]["other"]["command"], "x");
    assert_eq!(v["mcpServers"]["vorpal"]["command"], "/usr/local/bin/vorpal");
    let backups: Vec<_> = fs::read_dir(&base)
      .unwrap()
      .flatten()
      .filter(|e| e.file_name().to_string_lossy().contains("bak-vorpal"))
      .collect();
    assert_eq!(backups.len(), 1, "modification takes exactly one backup");

    // Invalid JSON is refused, unchanged.
    fs::write(base.join(".mcp.json"), "{ not json").unwrap();
    assert!(run_install(Client::ClaudeCode, Some("/x"), false).is_err());
    assert_eq!(
      fs::read_to_string(base.join(".mcp.json")).unwrap(),
      "{ not json"
    );

    std::env::set_current_dir(old).unwrap();
    let _ = fs::remove_dir_all(&base);
  }
}
