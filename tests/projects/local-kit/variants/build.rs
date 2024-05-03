use std::process::{exit, Command};

fn main() -> Result<(), std::io::Error> {
    let ret = Command::new("true").arg("build-variant").status()?;
    if !ret.success() {
        exit(1);
    }
    Ok(())
}
