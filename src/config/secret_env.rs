//! The one rule for a `lambo.toml` key that names the environment variable
//! holding a secret: `[[serve.credential]] token_env` (#32) and
//! `[embedder] api_key_env` (#21).
//!
//! Such a key is where a token is most likely to be pasted by mistake, so its
//! value must be a conventional upper-case variable name
//! ([`is_conventional_env_name`]) and must not look like a token
//! ([`looks_like_a_token`]). A value that fails either check is refused
//! without being quoted ([`check`] and [`SecretEnvRefusal::message`]), and
//! every later message that names the variable goes through [`shown`], so a
//! message can never become the leak.
//!
//! A leaf module: it depends on nothing in the crate, so both the config
//! layer and the embedder adapters can use it.

/// The longest variable name accepted. Real variable names are short; a long
/// value is far more likely a pasted token.
pub(crate) const MAX_SECRET_ENV_LEN: usize = 64;

/// Shown in place of a value that may be a secret.
pub(crate) const VALUE_NOT_SHOWN: &str = "(value not shown)";

/// Is `name` a conventional environment variable name: `[A-Z_][A-Z0-9_]*`,
/// at most [`MAX_SECRET_ENV_LEN`] bytes? Lower case is refused on purpose:
/// tokens are usually mixed or lower case, variable names upper case.
pub(crate) fn is_conventional_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_SECRET_ENV_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Does a value that passes [`is_conventional_env_name`] still look like a
/// token? An AWS access key id (`AKIA`/`ASIA` plus 16 characters), or any
/// 20-plus character run of letters and digits with no `_`, reads as random
/// rather than as a name.
pub(crate) fn looks_like_a_token(name: &str) -> bool {
    let aws_key_id = name.len() == 20 && (name.starts_with("AKIA") || name.starts_with("ASIA"));
    let random_run = name.len() >= 20
        && !name.contains('_')
        && name.bytes().any(|b| b.is_ascii_digit())
        && name.bytes().any(|b| b.is_ascii_uppercase());
    aws_key_id || random_run
}

/// Is `name` safe to quote in a message? Only a value that [`check`] accepts
/// is: anything else may be the token itself.
pub(crate) fn is_quotable(name: &str) -> bool {
    is_conventional_env_name(name) && !looks_like_a_token(name)
}

/// `name` for a message: the name when it is quotable, otherwise
/// [`VALUE_NOT_SHOWN`]. Validation already refuses unquotable values; this is
/// the second line.
pub(crate) fn shown(name: &str) -> &str {
    if is_quotable(name) {
        name
    } else {
        VALUE_NOT_SHOWN
    }
}

/// Why a value was refused as a variable name. Carries no part of the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecretEnvRefusal {
    /// Not `[A-Z_][A-Z0-9_]*` of at most [`MAX_SECRET_ENV_LEN`] bytes.
    NotAName,
    /// A conventional name that reads as a token.
    LooksLikeAToken,
}

impl SecretEnvRefusal {
    /// The refusal for the key `key` (for example `token_env`), introduced by
    /// `subject` (for example `credential "a": token_env`). Neither argument
    /// may be the refused value; the message never quotes it.
    pub(crate) fn message(self, subject: &str, key: &str) -> String {
        match self {
            Self::NotAName => format!(
                "{subject} is not an environment variable name {VALUE_NOT_SHOWN}: it must be \
                 [A-Z_][A-Z0-9_]*, at most {MAX_SECRET_ENV_LEN} bytes. Put the token in an \
                 environment variable and set {key} to that variable's name"
            ),
            Self::LooksLikeAToken => format!(
                "{subject} looks like a token rather than the name of the environment variable \
                 holding one {VALUE_NOT_SHOWN}. Put the token in an environment variable and set \
                 {key} to that variable's name"
            ),
        }
    }
}

/// Accept `name` only if it is a quotable variable name.
pub(crate) fn check(name: &str) -> Result<(), SecretEnvRefusal> {
    if !is_conventional_env_name(name) {
        Err(SecretEnvRefusal::NotAName)
    } else if looks_like_a_token(name) {
        Err(SecretEnvRefusal::LooksLikeAToken)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_names_pass_and_are_shown() {
        for name in ["CLOUDFLARE_API_TOKEN", "_X", "OPENAI_API_KEY", "A1_B2"] {
            assert_eq!(check(name), Ok(()), "{name}");
            assert_eq!(shown(name), name);
        }
        let longest = "A".repeat(MAX_SECRET_ENV_LEN);
        assert_eq!(check(&longest), Ok(()));
    }

    /// Mutation: drop either half of `is_quotable` -> red.
    #[test]
    fn refused_values_are_classified_and_never_shown() {
        // Built at run time so the source holds no key-id-shaped literal.
        let aws_shaped = format!("AKIA{}", "Q".repeat(16));
        let long = "A".repeat(MAX_SECRET_ENV_LEN + 1);
        for (bad, why) in [
            ("", SecretEnvRefusal::NotAName),
            ("lower_case", SecretEnvRefusal::NotAName),
            ("1ABC", SecretEnvRefusal::NotAName),
            ("WITH-DASH", SecretEnvRefusal::NotAName),
            ("fake-xyzzy-not-a-token", SecretEnvRefusal::NotAName),
            (long.as_str(), SecretEnvRefusal::NotAName),
            (aws_shaped.as_str(), SecretEnvRefusal::LooksLikeAToken),
            ("ABCDEFGHIJ0123456789XY", SecretEnvRefusal::LooksLikeAToken),
        ] {
            assert_eq!(check(bad), Err(why), "{bad:?}");
            assert_eq!(shown(bad), VALUE_NOT_SHOWN, "{bad:?}");
            let msg = why.message("some_key", "some_key");
            assert!(msg.contains(VALUE_NOT_SHOWN), "{msg}");
            if !bad.is_empty() {
                assert!(!msg.contains(bad), "{msg}");
            }
        }
    }
}
