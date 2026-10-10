//! The one rule for a `lambo.toml` key that names the environment variable
//! holding a secret: `[[serve.credential]] token_env` (#32),
//! `[[web.credential]] token_env` (#4 PR 3) and `[embedder] api_key_env`
//! (#21). Neither may name a variable lambo reads its
//! own credentials from ([`SERVE_AUTH_TOKEN_ENV`], [`LAMBO_CREDENTIAL_ENVS`]).
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

/// The variable `lambo serve` reads its own bearer token from (the legacy
/// `--auth-token` credential, named `default`). `crate::mcp::AUTH_TOKEN_ENV`
/// is defined as this constant, so the two cannot drift.
///
/// No key that names a secret's variable may name this one: for `token_env`
/// it would make a second credential out of the legacy one, and for
/// `api_key_env` it would send serve's own token to an embeddings endpoint.
pub(crate) const SERVE_AUTH_TOKEN_ENV: &str = "LAMBO_AUTH_TOKEN";

/// The variables lambo itself reads a credential from, other than
/// [`SERVE_AUTH_TOKEN_ENV`]: the store DSNs (which carry the database
/// password) and the Google credential-file variables. No key that names a
/// secret's variable may name one of these: `api_key_env` would send the DSN
/// or the credential path to an embeddings endpoint as a bearer token, and
/// `token_env` would make it a serve credential.
///
/// Spelled here because this module is a leaf; the test module pins each
/// entry to the constant or reader it mirrors, so a rename there fails it.
pub(crate) const LAMBO_CREDENTIAL_ENVS: &[&str] = &[
    // `crate::store::{COCKROACH,POSTGRES,FALLBACK}_DSN_ENV`
    "LAMBO_COCKROACH_DSN",
    "LAMBO_POSTGRES_DSN",
    "DATABASE_URL",
    // `gcp_auth::credentials_path_from_env` and the Gemini embedder
    "GCP_LAMBO_CREDENTIALS",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "LAMBO_GEMINI_CREDENTIALS",
];

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
    /// [`SERVE_AUTH_TOKEN_ENV`], which holds `lambo serve`'s own token.
    ReservedForServe,
    /// One of [`LAMBO_CREDENTIAL_ENVS`] (the name it is), which lambo reads
    /// one of its own credentials from.
    ReservedForLambo(&'static str),
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
            Self::ReservedForServe => format!(
                "{subject} may not be {SERVE_AUTH_TOKEN_ENV}, which holds lambo serve's own \
                 token (the legacy --auth-token credential, named \"default\"). Give {key} a \
                 variable of its own"
            ),
            Self::ReservedForLambo(name) => format!(
                "{subject} may not be {name}, which lambo reads one of its own credentials from \
                 (a store DSN or a Google credentials file). Give {key} a variable of its own"
            ),
        }
    }
}

/// Refuse `name` for the key `key` (introduced by `subject`) when it is also
/// the `token_env` of one of `lambo serve`'s `[[serve.credential]]` entries,
/// given as `(credential name, token_env)` pairs. One variable must not hold
/// both serve's credential and another secret: the other key's consumer (an
/// embeddings endpoint, say) would be handed serve's token. `name` is quoted
/// only through [`shown`].
pub(crate) fn check_not_a_serve_credential<'a>(
    name: &str,
    subject: &str,
    key: &str,
    serve_credentials: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), String> {
    match serve_credentials
        .into_iter()
        .find(|(_, token_env)| *token_env == name)
    {
        Some((credential, _)) => Err(format!(
            "{subject} names {}, which is also the token_env of [[serve.credential]] \
             {credential:?}: one variable must not hold both lambo serve's credential and \
             another secret. Give {key} a variable of its own",
            shown(name)
        )),
        None => Ok(()),
    }
}

/// [`check_not_a_serve_credential`] for `lambo serve-web`'s
/// `[[web.credential]]` entries (#4 PR 3): one variable must not hold both a
/// portal credential and another secret either.
pub(crate) fn check_not_a_web_credential<'a>(
    name: &str,
    subject: &str,
    key: &str,
    web_credentials: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), String> {
    match web_credentials
        .into_iter()
        .find(|(_, token_env)| *token_env == name)
    {
        Some((credential, _)) => Err(format!(
            "{subject} names {}, which is also the token_env of [[web.credential]] \
             {credential:?}: one variable must not hold both lambo serve-web's credential and \
             another secret. Give {key} a variable of its own",
            shown(name)
        )),
        None => Ok(()),
    }
}

/// Accept `name` only if it is a quotable variable name other than
/// [`SERVE_AUTH_TOKEN_ENV`] and the [`LAMBO_CREDENTIAL_ENVS`].
pub(crate) fn check(name: &str) -> Result<(), SecretEnvRefusal> {
    if !is_conventional_env_name(name) {
        Err(SecretEnvRefusal::NotAName)
    } else if looks_like_a_token(name) {
        Err(SecretEnvRefusal::LooksLikeAToken)
    } else if name == SERVE_AUTH_TOKEN_ENV {
        Err(SecretEnvRefusal::ReservedForServe)
    } else if let Some(&reserved) = LAMBO_CREDENTIAL_ENVS.iter().find(|&&r| r == name) {
        Err(SecretEnvRefusal::ReservedForLambo(reserved))
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

    /// Mutation: drop the `SERVE_AUTH_TOKEN_ENV` arm of `check` -> red.
    #[test]
    fn serves_own_variable_is_refused_by_name() {
        assert_eq!(crate::mcp::AUTH_TOKEN_ENV, SERVE_AUTH_TOKEN_ENV);
        assert_eq!(
            check(SERVE_AUTH_TOKEN_ENV),
            Err(SecretEnvRefusal::ReservedForServe)
        );
        let msg = SecretEnvRefusal::ReservedForServe.message("embedder.api_key_env", "api_key_env");
        assert!(msg.contains(SERVE_AUTH_TOKEN_ENV), "{msg}");
        assert!(msg.contains("api_key_env"), "{msg}");
    }

    /// Review L2 (#21): lambo's own credential variables are refused by name,
    /// for `api_key_env` and `token_env` alike, and the message names the
    /// variable and the key.
    ///
    /// Mutation: drop the `LAMBO_CREDENTIAL_ENVS` arm of `check` -> red.
    #[test]
    fn lambos_own_credential_variables_are_refused_by_name() {
        for &name in LAMBO_CREDENTIAL_ENVS {
            assert_eq!(
                check(name),
                Err(SecretEnvRefusal::ReservedForLambo(name)),
                "{name}"
            );
            let msg = SecretEnvRefusal::ReservedForLambo(name)
                .message("embedder.api_key_env", "api_key_env");
            assert!(msg.contains(name) && msg.contains("api_key_env"), "{msg}");
        }
    }

    /// The list mirrors what lambo actually reads: every entry is cleared by
    /// the hermetic-resolve list, and the DSN entries are the store's
    /// constants. A rename on either side fails here.
    #[test]
    fn lambos_credential_variables_match_what_lambo_reads() {
        for &name in LAMBO_CREDENTIAL_ENVS {
            assert!(
                crate::resolve::RESOLVE_ENV_VARS.contains(&name),
                "{name} is not a variable the resolve reads"
            );
        }
        for dsn in [
            crate::store::COCKROACH_DSN_ENV,
            crate::store::POSTGRES_DSN_ENV,
            crate::store::FALLBACK_DSN_ENV,
        ] {
            assert!(LAMBO_CREDENTIAL_ENVS.contains(&dsn), "{dsn}");
        }
        #[cfg(any(feature = "embed-gemini", feature = "store-postgres"))]
        {
            let env = crate::test_util::env_lock();
            for name in ["GCP_LAMBO_CREDENTIALS", "GOOGLE_APPLICATION_CREDENTIALS"] {
                assert!(LAMBO_CREDENTIAL_ENVS.contains(&name), "{name}");
                env.remove("GCP_LAMBO_CREDENTIALS");
                env.remove("GOOGLE_APPLICATION_CREDENTIALS");
                env.set(name, "/nonexistent/lambo-l2-pin.json");
                assert_eq!(
                    crate::gcp_auth::credentials_path_from_env(),
                    Some(std::path::PathBuf::from("/nonexistent/lambo-l2-pin.json")),
                    "{name}"
                );
            }
        }
    }

    /// Mutation: make `check_not_a_serve_credential` always `Ok` -> red.
    #[test]
    fn a_serve_credentials_variable_is_refused_naming_the_credential() {
        let creds = [("agents", "LAMBO_AGENTS_TOKEN"), ("ops", "LAMBO_OPS_TOKEN")];
        let err = check_not_a_serve_credential(
            "LAMBO_OPS_TOKEN",
            "embedder.api_key_env",
            "api_key_env",
            creds,
        )
        .unwrap_err();
        assert!(
            err.contains("\"ops\"") && err.contains("LAMBO_OPS_TOKEN"),
            "{err}"
        );
        assert!(err.contains("api_key_env"), "{err}");
        check_not_a_serve_credential("CLOUDFLARE_API_TOKEN", "s", "k", creds).unwrap();
    }
}
