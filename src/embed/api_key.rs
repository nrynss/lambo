//! `[embedder] api_key_env`: the bearer token for a hosted OpenAI-compatible
//! embeddings endpoint, by reference only (issue #21).
//!
//! The file names an environment variable; the token itself never goes in
//! `lambo.toml`. The rules mirror `[serve] token_env` (#32): a value that is not
//! a conventional variable name, or that looks like a token, is refused with
//! `(value not shown)` and never quoted, because `api_key_env` is the key a
//! token is most likely to be pasted into. An inline `api_key = "..."` is
//! accepted by the parser only so it can be refused with a pointer at
//! `api_key_env`; its value is discarded while parsing and never stored.
//!
//! Ungated: the config key parses in every build, whichever adapters are
//! compiled, so a file stays valid across feature rows.

use super::EmbedError;
use serde::{Deserialize, Deserializer};

/// The variable that overrides `embedder.api_key_env`. Like the key it
/// overrides, it holds a variable *name*, never a token.
pub const API_KEY_ENV_OVERRIDE: &str = "LAMBO_EMBED_API_KEY_ENV";

/// The longest `api_key_env` accepted. Real variable names are short; a long
/// value is far more likely a pasted token.
const MAX_API_KEY_ENV_LEN: usize = 64;

/// Shown in place of an `api_key_env` value that may be a secret.
const VALUE_NOT_SHOWN: &str = "(value not shown)";

/// An inline `api_key = "..."` in `[embedder]`. Deserializing it discards the
/// value, so a pasted token is never held in memory, printed by `Debug`, or
/// written back by `Serialize`; its only purpose is to be refused with a
/// pointer at `api_key_env`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineApiKey;

impl<'de> Deserialize<'de> for InlineApiKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Self)
    }
}

/// Is `name` a conventional environment variable name: `[A-Z_][A-Z0-9_]*`,
/// at most [`MAX_API_KEY_ENV_LEN`] bytes? Lower case is refused on purpose:
/// tokens are usually mixed or lower case, variable names upper case.
fn is_conventional_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_API_KEY_ENV_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Does a value that passes [`is_conventional_env_name`] still look like a
/// token? An AWS access key id (`AKIA`/`ASIA` plus 16 characters), or any
/// 20-plus character run of letters and digits with no `_`, reads as random
/// rather than as a name.
fn looks_like_a_token(name: &str) -> bool {
    let aws_key_id = name.len() == 20 && (name.starts_with("AKIA") || name.starts_with("ASIA"));
    let random_run = name.len() >= 20
        && !name.contains('_')
        && name.bytes().any(|b| b.is_ascii_digit())
        && name.bytes().any(|b| b.is_ascii_uppercase());
    aws_key_id || random_run
}

/// `api_key_env` for a message: the name when validation would accept it,
/// otherwise [`VALUE_NOT_SHOWN`]. Validation already refuses such values before
/// any later message is built; this is the second line, so a message can never
/// become the leak.
pub(crate) fn shown_env(name: &str) -> &str {
    if is_conventional_env_name(name) && !looks_like_a_token(name) {
        name
    } else {
        VALUE_NOT_SHOWN
    }
}

/// Refuse an inline key and an `api_key_env` that is not a quotable variable
/// name. Neither message quotes the offending value.
pub(crate) fn validate(
    api_key_env: Option<&str>,
    inline: Option<InlineApiKey>,
) -> Result<(), EmbedError> {
    if inline.is_some() {
        return Err(EmbedError::Unavailable(format!(
            "embedder.api_key is refused: a secret in lambo.toml ends up in backups, diffs and \
             support bundles. Put the token in an environment variable and name it with \
             api_key_env {VALUE_NOT_SHOWN}"
        )));
    }
    let Some(name) = api_key_env else {
        return Ok(());
    };
    if !is_conventional_env_name(name) {
        return Err(EmbedError::Unavailable(format!(
            "embedder.api_key_env is not an environment variable name {VALUE_NOT_SHOWN}: it \
             must be [A-Z_][A-Z0-9_]*, at most {MAX_API_KEY_ENV_LEN} bytes. Put the token in an \
             environment variable and set api_key_env to that variable's name"
        )));
    }
    if looks_like_a_token(name) {
        return Err(EmbedError::Unavailable(format!(
            "embedder.api_key_env looks like a token rather than the name of the environment \
             variable holding one {VALUE_NOT_SHOWN}. Put the token in an environment variable \
             and set api_key_env to that variable's name"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake, token-shaped value used only to prove it is never echoed.
    const FAKE_TOKEN: &str = "fake-xyzzy-not-a-token";

    #[test]
    fn conventional_names_pass_and_are_quotable() {
        for name in ["CLOUDFLARE_API_TOKEN", "_X", "OPENAI_API_KEY", "A1_B2"] {
            validate(Some(name), None).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(shown_env(name), name);
        }
        validate(None, None).unwrap();
    }

    /// Every refused `api_key_env` is refused without quoting it.
    ///
    /// Mutation: quote `name` in either refusal -> red.
    #[test]
    fn refused_names_are_never_quoted() {
        let long = "A".repeat(MAX_API_KEY_ENV_LEN + 1);
        // Built at run time so the source holds no key-id-shaped literal.
        let aws_shaped = format!("AKIA{}", "Q".repeat(16));
        for bad in [
            "",
            "cloudflare_api_token",
            "1ABC",
            "WITH-DASH",
            "WITH SPACE",
            FAKE_TOKEN,
            aws_shaped.as_str(),
            "ABCDEFGHIJ0123456789XY",
            long.as_str(),
        ] {
            let err = validate(Some(bad), None)
                .expect_err(&format!("{bad:?} must be refused"))
                .to_string();
            assert!(err.contains(VALUE_NOT_SHOWN), "{err}");
            assert!(err.contains("api_key_env"), "{err}");
            if !bad.is_empty() {
                assert!(!err.contains(bad), "the refusal quoted the value: {err}");
            }
            assert_eq!(shown_env(bad), VALUE_NOT_SHOWN);
        }
    }

    #[test]
    fn an_inline_key_is_refused_naming_api_key_env() {
        let err = validate(None, Some(InlineApiKey)).unwrap_err().to_string();
        assert!(err.contains("api_key_env"), "{err}");
        assert!(err.contains("embedder.api_key"), "{err}");
    }

    /// The inline value is discarded while parsing: neither `Debug` nor a
    /// re-serialization of the config can carry it.
    #[test]
    fn an_inline_key_never_reaches_debug_or_serialize() {
        let cfg: crate::embed::EmbedderConfig =
            toml::from_str(&format!("api_key = \"{FAKE_TOKEN}\"\n")).unwrap();
        assert_eq!(cfg.api_key, Some(InlineApiKey));
        assert!(!format!("{cfg:?}").contains(FAKE_TOKEN));
        let back = toml::to_string(&cfg).unwrap();
        assert!(!back.contains(FAKE_TOKEN), "{back}");
        assert!(!back.contains("api_key ="), "{back}");
    }
}
