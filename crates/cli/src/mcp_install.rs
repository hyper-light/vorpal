//! `vorpal mcp install` (D5): write the MCP client configuration for the common clients —
//! idempotent edits with a timestamped backup before any modification, `--dry-run` to
//! preview, and an explicit report of every file touched or skipped. Project-scoped files are
//! preferred wherever the client supports them (they travel with the repo and cannot clobber
//! a user's personal global config); global-only clients are edited in place with a backup.
//!
//! The entry always pins the index with an absolute path: MCP clients launch servers without
//! a working directory, so a relative `.vorpal/index` would resolve against whatever the
//! client happens to run in. Run the command from the project root you want served.

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
  /// Codex CLI — global `~/.codex/config.toml` (`[mcp_servers.vorpal]`).
  Codex,
  /// Cursor — project-scoped `.cursor/mcp.json`.
  Cursor,
  /// VS Code — project-scoped `.vscode/mcp.json` (note: uses a `servers` key).
  Vscode,
  /// Windsurf — global `~/.codeium/windsurf/mcp_config.json`.
  Windsurf,
  /// Every client whose location can be determined on this machine.
  All,
}

/// How a client's config file is shaped.
#[derive(Clone, Copy)]
enum Format {
  /// JSON with a top-level object of servers under `key` (`mcpServers` everywhere except
  /// VS Code's `servers`).
  Json { key: &'static str },
  /// TOML with a `[<table>.vorpal]` table (Codex).
  Toml { table: &'static str },
}

struct Target {
  client: &'static str,
  path: PathBuf,
  format: Format,
  /// Global configs are only written when the client is plausibly present (its config dir
  /// exists); project configs are always writable.
  global: bool,
}

/// Claude Desktop keeps its config in the platform's app-support directory.
fn claude_desktop_config(home: &Path) -> PathBuf {
  if cfg!(target_os = "macos") {
    home.join("Library/Application Support/Claude/claude_desktop_config.json")
  } else if cfg!(windows) {
    std::env::var_os("APPDATA")
      .map(PathBuf::from)
      .unwrap_or_else(|| home.join("AppData/Roaming"))
      .join("Claude/claude_desktop_config.json")
  } else {
    home.join(".config/Claude/claude_desktop_config.json")
  }
}

fn targets(client: Client, project_dir: &Path) -> Result<Vec<Target>> {
  let home = std::env::var_os("HOME")
    .or_else(|| std::env::var_os("USERPROFILE"))
    .map(PathBuf::from)
    .ok_or_else(|| anyhow!("cannot locate HOME"))?;
  let mut all = vec![
    Target {
      client: "claude-code",
      path: project_dir.join(".mcp.json"),
      format: Format::Json { key: "mcpServers" },
      global: false,
    },
    Target {
      client: "claude-desktop",
      path: claude_desktop_config(&home),
      format: Format::Json { key: "mcpServers" },
      global: true,
    },
    Target {
      client: "codex",
      path: home.join(".codex/config.toml"),
      format: Format::Toml {
        table: "mcp_servers",
      },
      global: true,
    },
    Target {
      client: "cursor",
      path: project_dir.join(".cursor").join("mcp.json"),
      format: Format::Json { key: "mcpServers" },
      global: false,
    },
    Target {
      client: "vscode",
      path: project_dir.join(".vscode").join("mcp.json"),
      format: Format::Json { key: "servers" },
      global: false,
    },
    Target {
      client: "windsurf",
      path: home.join(".codeium/windsurf/mcp_config.json"),
      format: Format::Json { key: "mcpServers" },
      global: true,
    },
  ];
  let wanted = |name: &str| match client {
    Client::All => true,
    Client::ClaudeCode => name == "claude-code",
    Client::ClaudeDesktop => name == "claude-desktop",
    Client::Codex => name == "codex",
    Client::Cursor => name == "cursor",
    Client::Vscode => name == "vscode",
    Client::Windsurf => name == "windsurf",
  };
  all.retain(|t| wanted(t.client));
  Ok(all)
}

/// The server entry every client receives: the absolute path of THIS binary (survives
/// PATH-less launchers) serving the current project's index at its absolute location.
fn server_entry(command_override: Option<&str>, project_dir: &Path) -> Result<Value> {
  let command = match command_override {
    Some(explicit) => explicit.to_string(),
    None => std::env::current_exe()
      .context("resolving the vorpal executable path")?
      .to_string_lossy()
      .into_owned(),
  };
  let index = project_dir.join(".vorpal").join("index");
  Ok(json!({"command": command, "args": ["mcp", "--index", index.to_string_lossy()]}))
}

pub fn run_install(client: Client, command_override: Option<&str>, dry_run: bool) -> Result<()> {
  let project_dir = std::env::current_dir().context("resolving the current directory")?;
  let entry = server_entry(command_override, &project_dir)?;
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
    let existing = fs::read_to_string(&target.path).ok();
    let rendered = match target.format {
      Format::Json { key } => render_json(&target, key, existing.as_deref(), &entry)?,
      Format::Toml { table } => render_toml(&target, table, existing.as_deref(), &entry)?,
    };
    let Some(rendered) = rendered else {
      println!(
        "ok    {:<14} {} (already configured)",
        target.client,
        target.path.display()
      );
      continue;
    };
    if dry_run {
      println!(
        "would {:<14} {}:\n{rendered}",
        target.client,
        target.path.display()
      );
      continue;
    }
    if let Some(parent) = target.path.parent() {
      fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if target.path.exists() {
      let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
      let ext = target
        .path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
      let backup = target
        .path
        .with_extension(format!("{ext}.bak-vorpal-{stamp}"));
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
  if matches!(client, Client::All | Client::ClaudeCode) {
    println!(
      "note  claude-code    defers MCP tool schemas behind ToolSearch (one model turn per \
       tool); set ENABLE_TOOL_SEARCH=false in Claude Code's env (e.g. .claude/settings.json \
       \"env\") to keep vorpal's schemas resident — see docs/mcp.md"
    );
  }
  Ok(())
}

/// The updated JSON text, or `None` when the file already holds exactly this entry.
fn render_json(
  target: &Target,
  key: &str,
  existing: Option<&str>,
  entry: &Value,
) -> Result<Option<String>> {
  let existing = match existing {
    Some(text) => serde_json::from_str::<Value>(text).with_context(|| {
      format!(
        "{} exists but is not valid JSON — fix or move it first (nothing was changed)",
        target.path.display()
      )
    })?,
    None => json!({}),
  };
  let mut updated = existing;
  let root = updated
    .as_object_mut()
    .ok_or_else(|| anyhow!("{} is not a JSON object", target.path.display()))?;
  let servers = root.entry(key).or_insert_with(|| Value::Object(Map::new()));
  let servers = servers
    .as_object_mut()
    .ok_or_else(|| anyhow!("{}.{} is not an object", target.path.display(), key))?;
  if servers.get("vorpal") == Some(entry) {
    return Ok(None);
  }
  servers.insert("vorpal".to_string(), entry.clone());
  Ok(Some(format!(
    "{}\n",
    serde_json::to_string_pretty(&updated)?
  )))
}

/// The updated TOML text (comments and unrelated tables preserved), or `None` when the
/// `[<table>.vorpal]` entry already matches.
fn render_toml(
  target: &Target,
  table: &str,
  existing: Option<&str>,
  entry: &Value,
) -> Result<Option<String>> {
  use toml_edit::{Array, DocumentMut, Item, Table, value};
  let mut doc: DocumentMut = match existing {
    Some(text) => text.parse().with_context(|| {
      format!(
        "{} exists but is not valid TOML — fix or move it first (nothing was changed)",
        target.path.display()
      )
    })?,
    None => DocumentMut::new(),
  };
  let command = entry["command"].as_str().unwrap_or_default();
  let args: Vec<&str> = entry["args"]
    .as_array()
    .map(|a| a.iter().filter_map(Value::as_str).collect())
    .unwrap_or_default();
  let current = doc
    .get(table)
    .and_then(Item::as_table)
    .and_then(|t| t.get("vorpal"))
    .and_then(Item::as_table);
  let same = current.is_some_and(|t| {
    t.get("command").and_then(Item::as_str) == Some(command)
      && t
        .get("args")
        .and_then(Item::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        == Some(args.clone())
  });
  if same {
    return Ok(None);
  }
  let mut server = Table::new();
  server["command"] = value(command);
  let mut list = Array::new();
  for arg in &args {
    list.push(*arg);
  }
  server["args"] = value(list);
  let servers = doc
    .entry(table)
    .or_insert(Item::Table(Table::new()))
    .as_table_mut()
    .ok_or_else(|| anyhow!("{}: [{table}] is not a table", target.path.display()))?;
  servers.set_implicit(true);
  servers["vorpal"] = Item::Table(server);
  Ok(Some(doc.to_string()))
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
    let project = std::env::current_dir().unwrap();

    // Dry run writes nothing.
    run_install(Client::ClaudeCode, Some("/usr/local/bin/vorpal"), true).unwrap();
    assert!(!base.join(".mcp.json").exists());

    // First real run writes; second is a no-op (idempotent). The index path is absolute.
    run_install(Client::ClaudeCode, Some("/usr/local/bin/vorpal"), false).unwrap();
    let text = fs::read_to_string(base.join(".mcp.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
      v["mcpServers"]["vorpal"]["command"],
      "/usr/local/bin/vorpal"
    );
    assert_eq!(v["mcpServers"]["vorpal"]["args"][0], "mcp");
    assert_eq!(v["mcpServers"]["vorpal"]["args"][1], "--index");
    assert_eq!(
      v["mcpServers"]["vorpal"]["args"][2].as_str(),
      project.join(".vorpal/index").to_str()
    );
    run_install(Client::ClaudeCode, Some("/usr/local/bin/vorpal"), false).unwrap();
    let backups: Vec<_> = fs::read_dir(&base)
      .unwrap()
      .flatten()
      .filter(|e| e.file_name().to_string_lossy().contains("bak-vorpal"))
      .collect();
    assert!(
      backups.is_empty(),
      "idempotent re-run must not touch the file"
    );

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
    assert_eq!(
      v["mcpServers"]["vorpal"]["command"],
      "/usr/local/bin/vorpal"
    );
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

  #[test]
  fn codex_toml_is_edited_in_place_and_idempotent() {
    let target = Target {
      client: "codex",
      path: PathBuf::from("/tmp/config.toml"),
      format: Format::Toml {
        table: "mcp_servers",
      },
      global: true,
    };
    let entry =
      json!({"command": "/usr/local/bin/vorpal", "args": ["mcp", "--index", "/p/.vorpal/index"]});
    let existing = "# my codex config\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
    let rendered = render_toml(&target, "mcp_servers", Some(existing), &entry)
      .unwrap()
      .expect("first install renders");
    assert!(
      rendered.starts_with("# my codex config\nmodel = \"o3\""),
      "{rendered}"
    );
    assert!(
      rendered.contains("[mcp_servers.other]\ncommand = \"x\""),
      "{rendered}"
    );
    assert!(rendered.contains("[mcp_servers.vorpal]"), "{rendered}");
    assert!(
      rendered.contains("args = [\"mcp\", \"--index\", \"/p/.vorpal/index\"]"),
      "{rendered}"
    );
    // Re-rendering the rendered text is a no-op.
    assert!(
      render_toml(&target, "mcp_servers", Some(&rendered), &entry)
        .unwrap()
        .is_none()
    );
    // Invalid TOML is refused.
    assert!(render_toml(&target, "mcp_servers", Some("= not toml"), &entry).is_err());
    // A fresh file gets exactly the one table.
    let fresh = render_toml(&target, "mcp_servers", None, &entry)
      .unwrap()
      .unwrap();
    assert!(
      fresh.trim_start().starts_with("[mcp_servers.vorpal]"),
      "{fresh}"
    );
  }
}
