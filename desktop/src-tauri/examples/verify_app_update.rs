//! CI-only pairing check: public material and the staged release archive only.
use base64::{engine::general_purpose::STANDARD, Engine};
use minisign_verify::{PublicKey, Signature};
fn verify() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("missing archive")?;
    let public =
        String::from_utf8(STANDARD.decode(std::env::var("SCIPORT_UPDATER_PUBLIC_KEY")?.trim())?)?;
    let signature = String::from_utf8(
        STANDARD.decode(std::fs::read_to_string(format!("{path}.sig"))?.trim())?,
    )?;
    PublicKey::decode(&public)?.verify(
        &std::fs::read(path)?,
        &Signature::decode(&signature)?,
        true,
    )?;
    Ok(())
}
fn main() {
    if verify().is_err() {
        eprintln!("release updater signature/public-key verification failed");
        std::process::exit(1);
    }
}
