//! `maraetai login` — the one-time credential-setup flow. Server URL and
//! username are typed plainly; the password is read with echo disabled
//! (`rpassword`) and goes straight to the OS keyring, never touching disk.

use std::io::{self, Write};

use anyhow::{Context, Result};
use maraetai_common::Credentials;

pub fn run() -> Result<()> {
    let server_url = prompt("Server URL (e.g. https://music.example.com)")?;
    let username = prompt("Username")?;
    let password = rpassword::prompt_password("Password: ").context("reading password")?;

    Credentials::save(server_url.clone(), username.clone(), &password)
        .context("saving credentials")?;

    println!(
        "Saved. Server: {server_url}, user: {username} (password stored in the OS keyring, not on disk)."
    );
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush().context("flushing prompt")?;
    let mut line = String::new();
    io::stdin().read_line(&mut line).context("reading input")?;
    Ok(line.trim().to_string())
}
