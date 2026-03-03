use anyhow::{bail, Context, Result};
use grammers_client::{Client, SignInError};
use std::io::{self, Write};

/// Run the interactive Telegram authentication flow:
///   1. Request a verification code to be sent to the user's device
///   2. Read the code from stdin
///   3. Sign in with the code
///   4. If 2FA is enabled, prompt for the password
///
/// All errors are context-rich with dynamic details about what went wrong.
pub async fn interactive_login(client: &Client, phone: &str, api_hash: &str) -> Result<()> {
    log::info!("Requesting login code for phone '{}'...", phone);

    let token = client
        .request_login_code(phone, api_hash)
        .await
        .with_context(|| {
            format!(
                "failed to request Telegram login code for phone '{}' — \
                 check that TELEGRAM_API_ID and TELEGRAM_API_HASH are correct \
                 (from https://my.telegram.org) and that the phone number is registered",
                phone
            )
        })?;

    let code = prompt_stdin("Enter the verification code Telegram sent to your device: ")
        .context("failed to read verification code from stdin")?;

    match client.sign_in(&token, code.trim()).await {
        Ok(_user) => {
            log::info!("Signed in successfully");
            Ok(())
        }

        Err(SignInError::PasswordRequired(password_token)) => {
            let hint = password_token.hint().unwrap_or("(no hint available)");
            log::info!("Two-factor authentication is enabled (hint: {})", hint);

            let password = prompt_stdin(&format!(
                "Enter your 2FA password (hint: '{}'): ",
                hint
            ))
            .context("failed to read 2FA password from stdin")?;

            client
                .check_password(password_token, password.trim())
                .await
                .context(
                    "2FA password verification failed — the password was rejected by Telegram",
                )?;

            log::info!("Signed in with 2FA successfully");
            Ok(())
        }

        Err(SignInError::InvalidCode) => {
            bail!(
                "verification code '{}' was rejected by Telegram as invalid — \
                 make sure you entered the most recent code (codes expire quickly)",
                code.trim()
            )
        }

        Err(SignInError::SignUpRequired) => {
            bail!(
                "phone number '{}' is not registered with Telegram — \
                 you must sign up using the official Telegram app first",
                phone
            )
        }

        Err(SignInError::InvalidPassword(_)) => {
            bail!(
                "the 2FA password was rejected by Telegram as incorrect — \
                 if you forgot your cloud password, reset it via the official Telegram app"
            )
        }

        Err(SignInError::Other(e)) => {
            bail!(
                "Telegram sign-in failed with unexpected error: {} — \
                 this may be a temporary server issue or a rate limit (FLOOD_WAIT)",
                e
            )
        }
    }
}

/// Prompt the user on stdout and read a line from stdin.
/// Fails with a clear error if stdin is closed (EOF) or unreadable.
fn prompt_stdin(prompt: &str) -> Result<String> {
    print!("{}", prompt);
    io::stdout()
        .flush()
        .context("failed to flush stdout before reading input")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read line from stdin — is stdin connected to a terminal?")?;

    if input.is_empty() {
        bail!(
            "stdin was closed (got EOF) while waiting for user input at prompt: '{}'",
            prompt.trim()
        );
    }

    Ok(input)
}
