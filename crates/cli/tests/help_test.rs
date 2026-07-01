mod common;

use anyhow::Result;
use assert_cmd::{Command, cargo_bin};
use common::create_test_files;
use predicates::str::contains;

#[test]
fn test_help_work_for_invalid_vorpalconfig() -> Result<()> {
  let dir = create_test_files([("vorpalconfig.yml", "invalid")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["help"])
    .assert()
    .success()
    .stdout(contains("vorpal"));
  Ok(())
}
