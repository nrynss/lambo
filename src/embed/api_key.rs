//! `[embedder] api_key_env`: the bearer token for a hosted OpenAI-compatible
//! embeddings endpoint, by reference only (issue #21).
//!
//! The file names an environment variable; the token itself never goes in
//! `lambo.toml`. The name rule is `[serve] token_env`'s (#32), shared in
//! `crate::config::secret_env`: a value that is not a conventional variable
//! name, or that looks like a token, is refused with `(value not shown)` and
//! never quoted, because `api_key_env` is the key a
//! token is most likely to be pasted into. An inline `api_key = "..."` is
//! accepted by the parser only so it can be refused with a pointer at
//! `api_key_env`; its value is discarded while parsing and never stored.
//!
//! Ungated: the config key parses in every build, whichever adapters are
//! compiled, so a file stays valid across feature rows.

use super::EmbedError;
use crate::config::secret_env::{self, VALUE_NOT_SHOWN};
use serde::{Deserialize, Deserializer};
#[cfg(feature = "embed-bge")]
use std::env;

/// The variable that overrides `embedder.api_key_env`. Like the key it
/// overrides, it holds a variable *name*, never a token.
pub const API_KEY_ENV_OVERRIDE: &str = "LAMBO_EMBED_API_KEY_ENV";

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
    secret_env::check(name)
        .map_err(|why| EmbedError::Unavailable(why.message("embedder.api_key_env", "api_key_env")))
}

/// Read the token from the variable `api_key_env` names, at resolve time.
///
/// Unset, empty or whitespace-only is a hard error naming the variable (never a
/// value): a configured key that silently sends no header would surface later
/// as a 401 on every write. Surrounding whitespace is trimmed, since a token
/// never contains any and a trailing newline from `$(cat file)` is not part of
/// it.
///
/// Only the `bge_m3` adapter sends a token, so this exists only where it is
/// compiled.
#[cfg(feature = "embed-bge")]
pub(crate) fn resolve(api_key_env: &str) -> Result<String, EmbedError> {
    validate(Some(api_key_env), None)?;
    let shown = secret_env::shown(api_key_env);
    match env::var(api_key_env) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        Ok(_) => Err(EmbedError::Unavailable(format!(
            "embedder.api_key_env names {shown}, which is set but empty; export the API token \
             in {shown} or remove api_key_env"
        ))),
        Err(env::VarError::NotPresent) => Err(EmbedError::Unavailable(format!(
            "embedder.api_key_env names {shown}, which is not set; export the API token in \
             {shown} or remove api_key_env"
        ))),
        Err(env::VarError::NotUnicode(_)) => Err(EmbedError::Unavailable(format!(
            "embedder.api_key_env names {shown}, which does not hold valid UTF-8 \
             {VALUE_NOT_SHOWN}"
        ))),
    }
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
            assert_eq!(secret_env::shown(name), name);
        }
        validate(None, None).unwrap();
    }

    /// Every refused `api_key_env` is refused without quoting it.
    ///
    /// Mutation: quote `name` in either refusal -> red.
    #[test]
    fn refused_names_are_never_quoted() {
        let long = "A".repeat(secret_env::MAX_SECRET_ENV_LEN + 1);
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
            assert_eq!(secret_env::shown(bad), VALUE_NOT_SHOWN);
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

    /// Resolve-time lookup: unset and empty are hard errors naming the
    /// variable; a set value is returned trimmed.
    #[cfg(feature = "embed-bge")]
    #[test]
    fn resolve_reads_the_named_variable() {
        const VAR: &str = "LAMBO_TEST_ISSUE21_EMBED_TOKEN";
        let env = crate::test_util::env_lock();

        env.remove(VAR);
        let err = resolve(VAR).unwrap_err().to_string();
        assert!(err.contains(VAR) && err.contains("not set"), "{err}");

        env.set(VAR, "   ");
        let err = resolve(VAR).unwrap_err().to_string();
        assert!(err.contains(VAR) && err.contains("empty"), "{err}");

        env.set(VAR, format!(" {FAKE_TOKEN}\n"));
        assert_eq!(resolve(VAR).unwrap(), FAKE_TOKEN);

        // A malformed name is refused before the environment is consulted.
        let err = resolve(FAKE_TOKEN).unwrap_err().to_string();
        assert!(!err.contains(FAKE_TOKEN), "{err}");
    }
}
