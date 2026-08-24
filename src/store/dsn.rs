//! Turning a Postgres-wire DSN *spelling* into a store *identity*.
//!
//! Lives in the store layer rather than beside the session endpoint because two
//! callers need the same rule and one of them is `StoreConfig::overlay_env`:
//!
//! * [`crate::mcp::endpoint`] derives a session's socket path from it, so two
//!   spellings of one database give one address (J2-R1-2 in Postgres clothes).
//! * [`crate::store::StoreConfig::overlay_env`] compares `store.dsn` against the
//!   kind's DSN environment variable with it, so "the config names the database,
//!   the environment supplies the credentials" keeps working while "the
//!   environment silently names a different database" is refused (E2E-F2).
//!
//! Password stripping is what makes the second caller possible: the canonical
//! form is safe to print in an error message. That has to hold on **every**
//! input, including the ones neither parser understands, which is what
//! `redact_unparseable_dsn` is for (B-E2E-R2-5, rebuilt as an allowlist for
//! B-E2E-R3-1: a shape this module cannot account for is not echoed at all).

/// Turn a Postgres-wire DSN *spelling* into a store *identity* (B1, J2-R1-2
/// wearing Postgres clothes).
///
/// `postgres://u@host/db` and `postgres://u@host:5432/db` are the same database
/// and different strings. Hashing them verbatim gave two serves on one machine
/// against one database two socket paths, so each believed it was alone.
///
/// # The rule
///
/// * **Scheme** `postgres` and `postgresql` are the same (emitted as
///   `postgres`).
/// * **Host** is lowercased. DNS is case-insensitive; two casings of one name
///   are one store.
/// * **Omitted port is 5432**, the libpq/sqlx default the driver will actually
///   dial. Cockroach's conventional 26257 is *not* the implicit port: sqlx
///   still dials 5432 when the DSN omits one, so identity follows the driver.
/// * **Omitted database defaults to the explicit username**, matching libpq
///   ("dbname defaults to the user name"). The OS user is never substituted:
///   identity must not depend on who launched the process (the DSN equivalent
///   of J2-R1-2's cwd trap).
/// * **Password is stripped.** Hashing already keeps it out of the filesystem
///   and the lease row; stripping means the pre-hash string is not a secret
///   either, and two credentials for one database still derive one endpoint.
/// * **Query parameters that do not name the database are dropped.**
///   `sslmode` / `sslrootcert` / `sslcert` / `sslkey` / `connect_timeout` /
///   `application_name` / `options` change how you connect, not which
///   database you open. `host` / `port` / `dbname` / `user` in the query
///   overlay the authority, matching sqlx.
/// * **Username is kept.** Two roles on one cluster can be two deployments;
///   the motivating example keeps `u`.
/// * **A non-URL DSN** (libpq `key=value`) is parsed for the same fields when
///   it contains `=`. Anything else is echoed only as far as a recognised
///   shape accounts for it, and is otherwise replaced wholesale: see
///   `redact_unparseable_dsn`.
pub(crate) fn canonical_store_dsn(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(parts) = parse_postgres_url(trimmed) {
        return parts.to_identity();
    }
    if let Some(parts) = parse_libpq_kv(trimmed) {
        return parts.to_identity();
    }
    redact_unparseable_dsn(trimmed)
}

/// What replaces a DSN whose shape this module cannot account for.
///
/// Deliberately a constant with no input in it: it cannot leak.
const UNPARSEABLE_DSN: &str = "<unparseable dsn>";

/// What replaces a URL query string that mentions a password.
const REDACTED_QUERY: &str = "?<query redacted>";

/// Last-resort redaction for a DSN neither parser understood (B-E2E-R2-5,
/// rebuilt as an allowlist for B-E2E-R3-1).
///
/// Every parsed path drops the password on the way to an identity, and
/// `StoreConfig::overlay_env` prints both sides of a disagreement under the
/// promise "(passwords stripped)". The unparseable path has to keep that
/// promise too, or the refusal prints a live credential on the line that says
/// it did not — the one place J2's hashing was built to keep passwords out of.
/// An operator reaches this path by ordinary typos: a port above 65535, a
/// non-numeric bracketed port, or a misspelled scheme (`postgre://`) all
/// defeat `parse_postgres_url`.
///
/// # Why this is an allowlist, and not a splice (B-E2E-R3-1)
///
/// The first fix spliced the secret *out*: find `://`, drop everything from the
/// first `:` of the userinfo to the last `@`. That is a blacklist, and a
/// blacklist has to enumerate every place a secret can hide in a string we have
/// already admitted we cannot parse. It did not, and could not. Measured on
/// this crate, five shapes walked straight through it with the password intact:
///
/// * `app:S3cret@127.0.0.1:26257/lambo` — the scheme dropped, so no `://`.
/// * `postgres:/app:S3cret@127.0.0.1:26257/lambo` — one missing keystroke, same.
/// * `postgres://app@127.0.0.1:70000/lambo?password=S3cret` — a `://` *and* an
///   `@`, and the secret behind both, in the query. libpq's own URI form.
/// * `postgres://127.0.0.1:70000/lambo?password=S3cret` — likewise, no userinfo.
/// * `host=h port=70000 user=app password = S3cret` — libpq allows spaces
///   around `=`, so the whitespace-token filter never sees a `password=` token.
///
/// The last three carry the `://` the repair for the first two would have
/// anchored on. There is no reason to believe a sixth does not exist, so the
/// burden is inverted: this function **builds** an echo out of pieces a
/// recognised shape positively accounts for, and anything it does not recognise
/// becomes [`UNPARSEABLE_DSN`], which cannot leak because no input reaches it.
///
/// # Why not a placeholder for everything
///
/// `canonical_store_dsn` is not only a printer, it is the identity function:
/// `store_identity` hashes it and `overlay_env` compares it. Collapsing every
/// unparseable spelling onto one constant would make two *different* malformed
/// DSNs compare equal, and E2E-F2's disagreement refusal would then let the
/// environment's DSN outrank the file's in silence. Echoing the shapes we do
/// recognise keeps the ordinary typo — which is both where the diagnostic value
/// is and where the identity distinction matters — safe *and* distinguishing.
/// The residue is real and accepted: two unrecognised spellings do collapse
/// onto one identity, and both are strings no driver will dial.
///
/// # The rule
///
/// Two shapes are recognised, and one gate applies to both.
///
/// * **URL-shaped**: `<scheme>://<rest>`, where `<scheme>` is an RFC 3986
///   scheme (`[A-Za-z][A-Za-z0-9+.-]*`) and `<rest>` holds no whitespace. The
///   userinfo is everything up to the **last** `@`, and only the part of it
///   before the first `:` is echoed. Using the last `@` rather than the one
///   that closes the authority over-redacts a DSN carrying an `@` in its path
///   or query, and over-redacting is the safe direction to be wrong in. A query
///   string that mentions a password is replaced wholesale by
///   [`REDACTED_QUERY`], so the operator still sees the host and port they
///   mistyped.
/// * **libpq `key=value`-shaped**: *every* whitespace-separated token is
///   `key=value` with an identifier key; tokens whose key mentions a password
///   are dropped. One token that is not `key=value` disqualifies the whole
///   string — that is what `password = S3cret` trips on.
/// * **The gate**: whatever a branch built is echoed only if it is non-empty,
///   mentions no password, and carries no control characters (an error message
///   goes to a terminal). Otherwise the placeholder ships. The keyword check is
///   a substring, so it also catches `sslpassword` and over-redacts a username
///   or `application_name` with "password" in it — again the safe direction.
fn redact_unparseable_dsn(raw: &str) -> String {
    match redact_url_shaped(raw).or_else(|| redact_kv_shaped(raw)) {
        Some(echo) if is_safe_to_echo(&echo) => echo,
        _ => UNPARSEABLE_DSN.to_string(),
    }
}

/// The one gate every echo passes through. Nothing leaves this module for an
/// error message without satisfying it.
fn is_safe_to_echo(echo: &str) -> bool {
    !echo.is_empty() && !echo.chars().any(char::is_control) && !mentions_password(echo)
}

fn mentions_password(s: &str) -> bool {
    s.to_ascii_lowercase().contains("password")
}

fn is_uri_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `<scheme>://[userinfo@]<tail>` — echo the scheme, the user, and the tail.
fn redact_url_shaped(raw: &str) -> Option<String> {
    let (scheme, after) = raw.split_once(':')?;
    if !is_uri_scheme(scheme) {
        return None;
    }
    let rest = after.strip_prefix("//")?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    let (user, tail) = match rest.rfind('@') {
        Some(at) => (rest[..at].split(':').next().unwrap_or(""), &rest[at..]),
        None => ("", rest),
    };
    let tail = match tail.split_once('?') {
        Some((before, query)) if mentions_password(query) => {
            format!("{before}{REDACTED_QUERY}")
        }
        _ => tail.to_string(),
    };
    Some(format!("{scheme}://{user}{tail}"))
}

/// libpq `key=value key=value` — echo every token whose key is not a password.
fn redact_kv_shaped(raw: &str) -> Option<String> {
    let mut kept = Vec::new();
    let mut saw_token = false;
    for token in raw.split_whitespace() {
        saw_token = true;
        let (key, _) = token.split_once('=')?;
        if !is_libpq_key(key) {
            return None;
        }
        if !mentions_password(key) {
            kept.push(token);
        }
    }
    saw_token.then(|| kept.join(" "))
}

fn is_libpq_key(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

struct DsnParts {
    user: String,
    host: String,
    port: u16,
    database: String,
}

impl DsnParts {
    fn to_identity(&self) -> String {
        let db = if self.database.is_empty() && !self.user.is_empty() {
            self.user.as_str()
        } else {
            self.database.as_str()
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.user.is_empty() {
            format!("postgres://{host}:{}/{}", self.port, db)
        } else {
            format!("postgres://{}@{host}:{}/{}", self.user, self.port, db)
        }
    }
}

const PG_DEFAULT_PORT: u16 = 5432;

fn parse_postgres_url(raw: &str) -> Option<DsnParts> {
    let lower = raw.to_ascii_lowercase();
    let rest = if lower.starts_with("postgres://") {
        &raw["postgres://".len()..]
    } else if lower.starts_with("postgresql://") {
        &raw["postgresql://".len()..]
    } else {
        return None;
    };
    let (authority, after) = split_authority(rest);
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    let user = match userinfo {
        Some(info) => {
            let u = info.split_once(':').map(|(u, _)| u).unwrap_or(info);
            percent_decode(u)
        }
        None => String::new(),
    };
    let (host, port) = split_host_port(hostport)?;
    let (path, query) = match after.strip_prefix('?') {
        Some(q) => ("", q),
        None => match after.split_once('?') {
            Some((p, q)) => (p, q),
            None => (after, ""),
        },
    };
    let mut database = path
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();
    if !database.is_empty() {
        database = percent_decode(&database);
    }
    let mut parts = DsnParts {
        user,
        host: host.to_ascii_lowercase(),
        port: port.unwrap_or(PG_DEFAULT_PORT),
        database,
    };
    apply_query_overlays(&mut parts, query);
    Some(parts)
}

fn split_authority(s: &str) -> (&str, &str) {
    let mut bracket = false;
    for (i, c) in s.char_indices() {
        match c {
            '[' => bracket = true,
            ']' => bracket = false,
            '/' | '?' if !bracket => return (&s[..i], &s[i..]),
            _ => {}
        }
    }
    (s, "")
}

fn split_host_port(hostport: &str) -> Option<(String, Option<u16>)> {
    if hostport.is_empty() {
        return Some((String::new(), None));
    }
    if let Some(rest) = hostport.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = rest[..end].to_string();
        let after = &rest[end + 1..];
        let port = if let Some(p) = after.strip_prefix(':') {
            Some(p.parse().ok()?)
        } else if after.is_empty() {
            None
        } else {
            return None;
        };
        return Some((host, port));
    }
    match hostport.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Some((percent_decode(h), Some(p.parse().ok()?)))
        }
        _ => Some((percent_decode(hostport), None)),
    }
}

fn apply_query_overlays(parts: &mut DsnParts, query: &str) {
    if query.is_empty() {
        return;
    }
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let key = percent_decode(k).to_ascii_lowercase();
        let value = percent_decode(v);
        match key.as_str() {
            "host" | "hostaddr" => parts.host = value.to_ascii_lowercase(),
            "port" => {
                if let Ok(p) = value.parse() {
                    parts.port = p;
                }
            }
            "dbname" | "database" => parts.database = value,
            "user" => parts.user = value,
            "password" => {} // stripped
            _ => {}          // ssl*, timeout, application_name, options, ...
        }
    }
}

fn parse_libpq_kv(raw: &str) -> Option<DsnParts> {
    if raw.contains("://") || !raw.contains('=') {
        return None;
    }
    let mut parts = DsnParts {
        user: String::new(),
        host: String::new(),
        port: PG_DEFAULT_PORT,
        database: String::new(),
    };
    let mut saw_identity_key = false;
    for token in raw.split_whitespace() {
        let (k, v) = match token.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let key = k.to_ascii_lowercase();
        match key.as_str() {
            "host" | "hostaddr" => {
                parts.host = v.to_ascii_lowercase();
                saw_identity_key = true;
            }
            "port" => {
                parts.port = v.parse().ok()?;
                saw_identity_key = true;
            }
            "dbname" | "database" => {
                parts.database = v.to_string();
                saw_identity_key = true;
            }
            "user" => {
                parts.user = v.to_string();
                saw_identity_key = true;
            }
            "password" => {}
            _ => {}
        }
    }
    saw_identity_key.then_some(parts)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "S3cretHunter";

    /// Every shape below defeats both parsers, so every one of them reaches
    /// `redact_unparseable_dsn` and gets printed under "(passwords stripped)".
    /// The first four are B-E2E-R2-5's filed shapes; the rest are
    /// B-E2E-R3-1's, and the label on each says which anchor it walks past.
    ///
    /// Kept in one place because the *universal* promise is tested over the
    /// whole list ([`an_unparseable_dsn_still_has_its_password_stripped`]) and
    /// the per-shape outcome is tested by the two tests after it.
    const UNPARSEABLE_SHAPES: &[&str] = &[
        // --- B-E2E-R2-5: a port past u16, a non-numeric bracketed port, a
        // misspelled scheme, a port past u16 with a query. All URL-shaped.
        "postgres://app:S3cretHunter@127.0.0.1:70000/lambo",
        "postgres://app:S3cretHunter@[::1]:notaport/lambo",
        "postgre://app:S3cretHunter@db.internal:26257/lambo",
        "postgresql://app:S3cretHunter@127.0.0.1:99999/lambo?sslmode=require",
        // --- B-E2E-R3-1, filed: no `://` for the splice to anchor on.
        "app:S3cretHunter@127.0.0.1:26257/lambo",
        "postgres:/app:S3cretHunter@127.0.0.1:26257/lambo",
        // --- B-E2E-R3-1, derived while fixing it. The point of these is that
        // extending the `://` anchor would NOT have closed them: the first two
        // carry a `://` and an `@` and put the secret behind both, in libpq's
        // own URI query form; the third is libpq's documented tolerance of
        // spaces around `=`, which the whitespace-token filter never saw.
        "postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter",
        "postgres://127.0.0.1:70000/lambo?password=S3cretHunter",
        "host=127.0.0.1 port=70000 dbname=lambo user=app password = S3cretHunter",
        // --- and the same class as the filed two, one case change away.
        "APP:S3cretHunter@127.0.0.1:70000/lambo",
        // --- the secret in the *username* position, where truncating the
        // userinfo at its first `:` leaves it whole.
        "postgres://password=S3cretHunter@127.0.0.1:70000/lambo",
        // --- an `@` after the authority, which is what the last-`@` rule
        // over-redacts rather than mis-splits.
        "postgres://app:S3cretHunter@127.0.0.1:70000/lam@bo",
        // --- a `://` inside the password, which moves the first-`://` anchor.
        "postgres://app:pa://S3cretHunter@127.0.0.1:70000/lambo",
        // --- an ANSI escape, which an error message hands to a terminal.
        "postgres://app:S3cretHunter@127.0.0.1:70000/lam\u{1b}[2Jbo",
    ];

    /// B-E2E-R2-5 and B-E2E-R3-1: the canonical form is printed under
    /// "(passwords stripped)", so the promise has to hold on inputs the
    /// parsers reject too — on **every** one of them, not on the ones we
    /// happened to think of.
    ///
    /// To watch this go red, make `redact_unparseable_dsn` splice again
    /// instead of allowlisting:
    ///
    /// ```ignore
    /// fn redact_unparseable_dsn(raw: &str) -> String {
    ///     let Some(sep) = raw.find("://") else { return raw.to_string() };
    ///     let (scheme, rest) = raw.split_at(sep + "://".len());
    ///     let Some(at) = rest.rfind('@') else { return raw.to_string() };
    ///     let (userinfo, from_at) = rest.split_at(at);
    ///     format!("{scheme}{}{from_at}", userinfo.split(':').next().unwrap_or(""))
    /// }
    /// ```
    #[test]
    fn an_unparseable_dsn_still_has_its_password_stripped() {
        for raw in UNPARSEABLE_SHAPES {
            assert!(
                parse_postgres_url(raw).is_none() && parse_libpq_kv(raw).is_none(),
                "{raw} is supposed to be the unparseable case; if it now parses, \
                 pick another shape rather than deleting the test"
            );
            let canon = canonical_store_dsn(raw);
            assert!(
                !canon.contains(SECRET),
                "the canonical form is printed under \"(passwords stripped)\", \
                 and this one still carries the password: {raw} -> {canon}"
            );
            assert!(
                is_safe_to_echo(&canon) || canon == UNPARSEABLE_DSN,
                "an echo that fails its own gate must have been replaced by the \
                 placeholder: {raw} -> {canon}"
            );
        }
    }

    /// The half of the promise that is not about safety: an operator who
    /// mistyped a port still has to be able to see the port they mistyped, or
    /// there was never any reason to quote the DSN in the message at all.
    #[test]
    fn a_recognised_shape_still_shows_the_operator_the_typo() {
        // The exact transcript from the round-2 review, unchanged by round 3.
        assert_eq!(
            canonical_store_dsn("postgres://app:S3cretHunter@127.0.0.1:70000/lambo"),
            "postgres://app@127.0.0.1:70000/lambo"
        );
        assert_eq!(
            canonical_store_dsn("postgres://app:S3cretHunter@[::1]:notaport/lambo"),
            "postgres://app@[::1]:notaport/lambo"
        );
        assert_eq!(
            canonical_store_dsn("postgre://app:S3cretHunter@db.internal:26257/lambo"),
            "postgre://app@db.internal:26257/lambo"
        );
        assert_eq!(
            canonical_store_dsn(
                "postgresql://app:S3cretHunter@127.0.0.1:99999/lambo?sslmode=require"
            ),
            "postgresql://app@127.0.0.1:99999/lambo?sslmode=require"
        );

        // A query that mentions a password loses the query, not the authority:
        // the mistyped port is still legible.
        assert_eq!(
            canonical_store_dsn("postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter"),
            "postgres://app@127.0.0.1:70000/lambo?<query redacted>"
        );
        assert_eq!(
            canonical_store_dsn("postgres://127.0.0.1:70000/lambo?password=S3cretHunter"),
            "postgres://127.0.0.1:70000/lambo?<query redacted>"
        );

        // A clean libpq keyword string keeps every token but the secret, even
        // when a bad port stopped it parsing.
        assert_eq!(
            canonical_store_dsn(
                "host=127.0.0.1 port=70000 dbname=lambo user=app password=S3cretHunter"
            ),
            "host=127.0.0.1 port=70000 dbname=lambo user=app"
        );

        // The last-`@` rule the round-3 review verified: an `@` in the path
        // over-redacts rather than mis-splitting, which is the safe direction.
        assert_eq!(
            canonical_store_dsn("postgres://app:S3cretHunter@127.0.0.1:70000/lam@bo"),
            "postgres://app@bo"
        );

        // Two credentials for one unparseable spelling are still one identity,
        // which is the same rule the parsed paths follow.
        assert_eq!(
            canonical_store_dsn("postgres://app:one@127.0.0.1:70000/lambo"),
            canonical_store_dsn("postgres://app:two@127.0.0.1:70000/lambo")
        );
    }

    /// The other half: a shape no branch accounts for is not echoed at all.
    ///
    /// This is the B-E2E-R3-1 decision in one test. The alternative on offer
    /// was to extend the `://` splice; the first three rows below are why it
    /// was rejected, because they carry a `://` and leak anyway.
    #[test]
    fn an_unrecognised_shape_is_replaced_wholesale() {
        for raw in [
            // filed: the anchor is simply absent
            "app:S3cretHunter@127.0.0.1:26257/lambo",
            "postgres:/app:S3cretHunter@127.0.0.1:26257/lambo",
            "APP:S3cretHunter@127.0.0.1:70000/lambo",
            // derived: libpq's spaces around `=` break the token shape
            "host=127.0.0.1 port=70000 dbname=lambo user=app password = S3cretHunter",
            // derived: the secret sits in the username position
            "postgres://password=S3cretHunter@127.0.0.1:70000/lambo",
            // derived: a control character has no business in a message that
            // is about to be written to a terminal
            "postgres://app:S3cretHunter@127.0.0.1:70000/lam\u{1b}[2Jbo",
            // a token that is not `key=value` disqualifies the kv shape; the
            // pre-R3 code answered "weird-thing" here, which was only safe
            // because this particular spacing put the secret in a token it
            // recognised
            "weird-thing password=S3cretHunter",
            // nothing survives the password filter, so there is nothing to say
            "password=S3cretHunter",
        ] {
            assert_eq!(
                canonical_store_dsn(raw),
                UNPARSEABLE_DSN,
                "no branch accounts for {raw}, so nothing from it may be echoed"
            );
        }

        // The price of the choice, pinned so that paying it stays deliberate:
        // two unrecognised spellings collapse onto one identity, so
        // `overlay_env` cannot tell them apart. Both are strings no driver
        // will dial. If a future change makes the placeholder distinguishing,
        // this assertion is the one to rewrite.
        assert_eq!(
            canonical_store_dsn("app:one@host-a:26257/db-a"),
            canonical_store_dsn("app:two@host-b:26257/db-b")
        );
    }

    /// The parsed paths were already safe; pinned here so both halves of the
    /// promise sit in one place.
    #[test]
    fn a_parseable_dsn_has_its_password_stripped() {
        assert_eq!(
            canonical_store_dsn("postgres://app:S3cretHunter@127.0.0.1:5432/lambo"),
            "postgres://app@127.0.0.1:5432/lambo"
        );
        assert_eq!(
            canonical_store_dsn(
                "host=127.0.0.1 port=5432 dbname=lambo user=app password=S3cretHunter"
            ),
            "postgres://app@127.0.0.1:5432/lambo"
        );
    }
}
