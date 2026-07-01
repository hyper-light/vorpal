// The alias command `vp` redirects everything to vorpal
// we need this to avoid "multiple build target" warning
// See https://github.com/rust-lang/cargo/issues/5930
fn main() -> std::io::Result<()> {
  // redirect to vorpal
  use std::env::args;
  use std::process::{Command, Stdio};
  let mut child = Command::new("vorpal")
    .args(args().skip(1))
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .spawn()?;
  let status = child.wait()?;
  std::process::exit(status.code().unwrap_or(1))
}
