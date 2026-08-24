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
//!
//! # Two jobs, two functions (B-E2E-R4-2)
//!
//! Rounds 2 and 3 fused those two callers into one function, and that fusion is
//! what R4-2 broke open. **Printing** wants an answer that cannot leak, which
//! for a shape we cannot parse means saying nothing about it. **Identity** wants
//! an answer that never makes two different databases look like one, which for
//! the same shape means saying something *different* about each. Those are
//! opposite requirements on one return value, and round 3 resolved them by
//! trading: it collapsed unrecognised spellings onto one constant and defended
//! the collapse with "no driver will dial them". Round 4 measured that against
//! `sqlx::postgres::PgConnectOptions::from_str` — the crate's actual dial path,
//! `store::pg::PgStore::connect_options` — and it is false. sqlx validates no
//! scheme at all, so `app:one@host-a:26257/db-a` dials, and
//! `postgres://app@host-a:/db_password_a` dials *host-a* while its host-b twin
//! dials *host-b*, with both collapsing onto the same constant here.
//!
//! So the two jobs are two functions now, and neither pays for the other:
//!
//! * [`store_dsn_echo`] is what a human is shown. It never widens beyond what a
//!   recognised shape accounts for, and answers [`UNPARSEABLE_DSN`] otherwise.
//! * [`store_dsn_identity`] is what is compared and hashed, and is never
//!   printed. It answers a digest of the input for a shape it cannot parse, so
//!   two spellings collapse only if they are the same string.
//!
//! `overlay_env` compares the identities and prints the echoes, which is how a
//! refusal can both fire and stay quotable.

use sha2::{Digest, Sha256};

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
///   it contains `=`. Anything else gets a digest of itself, so that two
///   spellings are one identity only when they are one string: see
///   `unaccounted_identity`.
///
/// # Not for printing
///
/// This is the *comparison* half of the module. It is hashed by
/// `store_identity` and compared by `overlay_env`, and neither prints it; the
/// unparseable answer is a digest, which is meaningless to an operator.
/// [`store_dsn_echo`] is the half that gets shown (B-E2E-R4-2).
pub(crate) fn store_dsn_identity(raw: &str) -> String {
    match canonicalize(raw) {
        Canonical::Absent => String::new(),
        Canonical::Parsed(identity) => identity,
        Canonical::Unaccounted(trimmed) => unaccounted_identity(trimmed),
    }
}

/// What an operator is shown when a message has to quote a DSN.
///
/// The counterpart to [`store_dsn_identity`] and the reason the two are not one
/// function (B-E2E-R4-2). Every string this returns is safe to put after
/// "(passwords stripped)": either a canonical form assembled from four fields
/// none of which is the password, or an echo built only from pieces a
/// recognised shape accounts for, or [`UNPARSEABLE_DSN`], which contains no
/// input at all.
///
/// Two different DSNs may well produce the same answer here. That is the whole
/// point of the split: withholding is safe for a *printer* and catastrophic for
/// an *identity*, so only this half is allowed to withhold.
pub(crate) fn store_dsn_echo(raw: &str) -> String {
    match canonicalize(raw) {
        Canonical::Absent => String::new(),
        // A parsed identity carries no password by construction — it is built
        // from user/host/port/database and `password` is dropped in both
        // parsers — so it needs no keyword gate. It does still reach a
        // terminal, and a percent-encoded escape in a database name would
        // arrive intact, so the control-character clause applies here too.
        Canonical::Parsed(identity) if is_terminal_safe(&identity) => identity,
        Canonical::Parsed(_) => UNPARSEABLE_DSN.to_string(),
        Canonical::Unaccounted(trimmed) => redact_unparseable_dsn(trimmed),
    }
}

/// What the two parsers made of an input, before either caller decides what to
/// do about it. Borrows the trimmed input so the identity half can digest the
/// exact bytes the operator wrote.
enum Canonical<'a> {
    /// No DSN at all (a `path`-shaped store, or an empty `store.dsn`).
    Absent,
    /// A shape one of the two parsers accounts for, as its identity.
    Parsed(String),
    /// A shape neither parser accounts for, as the trimmed input.
    Unaccounted(&'a str),
}

fn canonicalize(raw: &str) -> Canonical<'_> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Canonical::Absent;
    }
    if let Some(parts) = parse_postgres_url(trimmed) {
        return Canonical::Parsed(parts.to_identity());
    }
    if let Some(parts) = parse_libpq_kv(trimmed) {
        return Canonical::Parsed(parts.to_identity());
    }
    Canonical::Unaccounted(trimmed)
}

/// The identity of a spelling neither parser accounts for (B-E2E-R4-2).
///
/// # Why a digest and not the placeholder
///
/// Round 3 gave every such spelling the constant [`UNPARSEABLE_DSN`], which
/// made two malformed DSNs compare equal in `overlay_env`: it then found
/// `from_file == from_env`, skipped the disagreement refusal, and let the
/// environment's DSN outrank the file's in silence — E2E-F2, the P1 that
/// refusal exists to prevent. The concession was defended on the grounds that
/// such strings never dial. Measured against sqlx, they do:
/// `postgres://app@host-a:/db_password_a` and its host-b twin collapsed onto
/// one constant here while `PgConnectOptions::from_str` resolved them to
/// `host-a`/`db_password_a` and `host-b`/`db_password_b`.
///
/// A digest cannot collapse two different strings (SHA-256, truncated to 128
/// bits, so a collision is not something a typo finds and not something an
/// operator who already owns the config file would need). It also carries no
/// substring of its input, which is why it is safe to hand to `store_identity`
/// and hash into a socket path: the sentence "the password never appears in the
/// string that is hashed" survives the change.
///
/// # Why it is not printed
///
/// It would be useless to an operator and it is derived from a credential.
/// `overlay_env` prints [`store_dsn_echo`] instead, which is why that function
/// exists. Nothing in this crate renders this value to a terminal, a log, or a
/// filesystem path except as the input to a further hash.
fn unaccounted_identity(trimmed: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(trimmed.as_bytes()));
    format!("{UNACCOUNTED_IDENTITY_PREFIX}{}>", &digest[..32])
}

/// What replaces a DSN this module will not quote.
///
/// Deliberately a constant with no input in it: it cannot leak. Two things
/// reach it — a shape neither parser accounts for and no branch of
/// `redact_unparseable_dsn` can rebuild, and (rarely) a parsed identity that is
/// not safe to hand a terminal. It is an *echo*, never an identity.
const UNPARSEABLE_DSN: &str = "<unparseable dsn>";

/// Prefix of [`unaccounted_identity`]'s answer. Shaped like the placeholder so
/// that a stray one showing up in a message is recognisable as this module's
/// doing, but distinct so the two can never be confused for each other.
const UNACCOUNTED_IDENTITY_PREFIX: &str = "<unparseable dsn#";

/// The sentence a refusal needs when one of its two quotes is the placeholder.
///
/// Without it, a disagreement between two spellings this module could not parse
/// reads "the config file says `<unparseable dsn>` and LAMBO_POSTGRES_DSN says
/// `<unparseable dsn>`" — two identical quotes under a claim that they differ,
/// which invites the operator to conclude the refusal is a bug. It is not: the
/// comparison ran on [`store_dsn_identity`], which does not collapse, and only
/// the *quoting* withheld. Empty when both sides were quotable, so the ordinary
/// message is unchanged.
pub(crate) fn withheld_note(from_file: &str, from_env: &str) -> &'static str {
    if from_file == UNPARSEABLE_DSN || from_env == UNPARSEABLE_DSN {
        " `<unparseable dsn>` is not a quotation: it stands for a spelling this \
         cannot parse, which is withheld rather than echoed so that \"(passwords \
         stripped)\" stays true of it. Two of them are still two different \
         spellings — the comparison ran on the strings themselves, not on this \
         placeholder."
    } else {
        ""
    }
}

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
/// # Why a placeholder for everything is fine *here* (corrected, B-E2E-R4-2)
///
/// Round 3 kept the recognised-shape echoes partly for diagnostics and partly
/// because this function was also the identity function, so collapsing
/// everything onto one constant would have made two *different* malformed DSNs
/// compare equal and let E2E-F2's refusal fall silent. It defended the residual
/// collapse with "both are strings no driver will dial", **which is false** —
/// see [`unaccounted_identity`] for the sqlx measurement that falsifies it.
///
/// The identity concern is gone from this function: [`store_dsn_identity`] no
/// longer routes through it, so collapsing here costs diagnostics and nothing
/// else. The recognised-shape echoes are kept for the diagnostics alone, which
/// is the honest reason and the only one that survives.
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
///   `key=value` with an identifier key, and only tokens whose key is on
///   [`is_echoable_libpq_key`]'s list are echoed. One token that is not
///   `key=value` disqualifies the whole string — that is what
///   `password = S3cret` trips on.
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

/// The gate on an echo rebuilt from a string neither parser understood.
///
/// Nothing leaves `redact_unparseable_dsn` without satisfying it. The keyword
/// clause is here and not on the parsed path because it stands in for a proof
/// we do not have: on a string we could not parse we cannot say where the
/// secret is, so we refuse anything that so much as mentions one. On the parsed
/// path we *can* say — the identity is four fields and none of them is the
/// password — so only [`is_terminal_safe`] applies there.
fn is_safe_to_echo(echo: &str) -> bool {
    is_terminal_safe(echo) && !mentions_password(echo)
}

/// The clause that holds on **every** echo, parsed or not: an error message is
/// handed to a terminal, and an ANSI escape smuggled through a percent-encoded
/// database name has no business being replayed there.
fn is_terminal_safe(echo: &str) -> bool {
    !echo.is_empty() && !echo.chars().any(char::is_control)
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

/// libpq `key=value key=value` — echo the tokens whose key is on the list.
///
/// # B-E2E-R4-1: this used to be a blacklist wearing an allowlist's name
///
/// It kept every token whose key merely *lacked* the substring "password", so a
/// secret parked under `passwrod=` (one transposition), `pwd=`, `pass=` or
/// `secret_pw=` printed verbatim under "(passwords stripped)". The trigger is
/// R2-5's own typo class: `port=70000` makes `parse_libpq_kv` fail, which is
/// what forces an otherwise-fine libpq string down here in the first place.
///
/// The round-3 defence — such a string is not a DSN, libpq rejects unknown
/// options, it would never dial — does not apply, because the leak is on the
/// *refusal* path, which fires precisely because the config is broken. R2-5's
/// own filed shapes never dial either. So the key set is now positive:
/// everything not on it is dropped, including keys this module has never heard
/// of, which is the only version of "allowlist" that means anything.
fn redact_kv_shaped(raw: &str) -> Option<String> {
    let mut kept = Vec::new();
    let mut saw_token = false;
    for token in raw.split_whitespace() {
        saw_token = true;
        let (key, _) = token.split_once('=')?;
        if !is_libpq_key(key) {
            return None;
        }
        if is_echoable_libpq_key(key) {
            kept.push(token);
        }
    }
    saw_token.then(|| kept.join(" "))
}

/// The libpq connection keywords whose *value* is a place, a mode or a name —
/// never a credential.
///
/// Membership is the whole safety argument of the kv branch, so the list is
/// deliberately short and deliberately positive. Dropping a key that belongs
/// here costs an operator some diagnostic detail; admitting one that does not
/// costs them their password, which is the asymmetry that decides every
/// borderline case below.
///
/// Off the list on purpose: `password` and `sslpassword` (secrets, the reason
/// this exists), `passfile` (a path to secrets), `options` (free-form text
/// libpq forwards to the backend, so it can hold anything), and every keyword
/// not enumerated — which is the point, because R4-1 was exactly the case of an
/// unenumerated key being kept.
fn is_echoable_libpq_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "host"
            | "hostaddr"
            | "port"
            | "dbname"
            | "database"
            | "user"
            | "sslmode"
            | "sslrootcert"
            | "sslcert"
            | "sslkey"
            | "connect_timeout"
            | "application_name"
            | "fallback_application_name"
            | "target_session_attrs"
            | "client_encoding"
    )
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
    // B-E2E-R4-3: an unencoded `:` in the path means an authority was cut in
    // half. `split_authority` stops at the first `/` or `?`, per RFC 3986 — so
    // `postgres://ap/p:S3cret@h:70000/db` has authority `ap` and everything
    // after it, password included, becomes the *database* component and is
    // echoed verbatim under "(passwords stripped)". A database name really
    // containing a colon is spelled `%3A` and still parses; sqlx rejects the
    // unencoded form outright ("invalid port number") for the sibling shapes,
    // so refusing here is the driver's answer, not a new opinion.
    if path.contains(':') {
        return None;
    }
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
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Some((percent_decode(h), Some(p.parse().ok()?)))
        }
        // B-E2E-R4-3: a `:` that is not a port separator is not part of a host.
        // The old fallback swallowed the whole thing as a hostname, so
        // `postgres://app:S3cret/Hunter@h/db` — where an unencoded `/` cut the
        // userinfo off before its `@` — canonicalised to
        // `postgres://[app:s3cret]:5432/…`, printing the password's first half
        // as a host. sqlx answers "invalid port number" to exactly these; this
        // arm is that answer.
        Some(_) => None,
        None => Some((percent_decode(hostport), None)),
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

    /// `S3cretHunter`, the password every shape below carries, plus its two
    /// halves — folded to lowercase, because an identity lowercases its host.
    ///
    /// B-E2E-R4-3 is why this list exists: `postgres://app:S3cret/Hunter@h/db`
    /// leaked as `postgres://[app:s3cret]:5432/Hunter@h/db`, which contains
    /// neither `S3cretHunter` (the password is split across two components)
    /// **nor** `s3cret` under a case-sensitive test (the host is lowercased on
    /// the way to an identity). Round 3's `!canon.contains(SECRET)` was true of
    /// that string. Half a password in a refusal message is a leaked password,
    /// so the check is fragments, folded to lowercase.
    const SECRET_FRAGMENTS: &[&str] = &["s3crethunter", "s3cret", "hunter"];

    /// Every string this module hands out, on every path, must survive this.
    fn assert_no_secret(what: &str, raw: &str, produced: &str) {
        let folded = produced.to_ascii_lowercase();
        for fragment in SECRET_FRAGMENTS {
            assert!(
                !folded.contains(fragment),
                "{what} carries {fragment:?} of the password: {raw} -> {produced}"
            );
        }
    }

    /// Every shape below defeats both parsers, so every one of them reaches
    /// `redact_unparseable_dsn` (for the echo) and `unaccounted_identity` (for
    /// the identity). The first four are B-E2E-R2-5's filed shapes; the next
    /// batch is B-E2E-R3-1's, and the last two batches are round 4's.
    ///
    /// Kept in one place because the *universal* promise is tested over the
    /// whole list ([`an_unparseable_dsn_still_has_its_password_stripped`]) and
    /// the per-shape outcome is tested by the tests after it.
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
        // --- B-E2E-R4-1: a bad port forces a libpq string down here, and the
        // secret then sits under a key that does not spell "password". Every
        // one of these printed verbatim under the round-3 blacklist.
        "host=127.0.0.1 port=70000 user=app passwrod=S3cretHunter",
        "host=127.0.0.1 port=70000 user=app pwd=S3cretHunter",
        "host=127.0.0.1 port=70000 user=app pass=S3cretHunter",
        "host=127.0.0.1 port=70000 user=app secret_pw=S3cretHunter",
        "host=127.0.0.1 port=70000 user=app sslpassword=S3cretHunter",
        // --- B-E2E-R4-3: an unencoded `/` or `?` in the userinfo cut the
        // authority short, and the password landed in the *database* or *host*
        // component of a supposedly-parsed identity. These parsed before this
        // round; the two guards in `parse_postgres_url` send them here.
        "postgres://ap/p:S3cretHunter@h:70000/db",
        "postgres://app:S3cret/Hunter@h/db",
        "postgres://app:S3cret?Hunter@h/db",
    ];

    /// B-E2E-R2-5, R3-1 and R4-3: the echo is printed under "(passwords
    /// stripped)", so the promise has to hold on inputs the parsers reject too
    /// — on **every** one of them, not on the ones we happened to think of.
    /// The identity is hashed into a socket path, so it has to hold there too.
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

            let echo = store_dsn_echo(raw);
            assert_no_secret("the echo", raw, &echo);
            assert!(
                is_safe_to_echo(&echo) || echo == UNPARSEABLE_DSN,
                "an echo that fails its own gate must have been replaced by the \
                 placeholder: {raw} -> {echo}"
            );

            // The identity is hashed into a socket path (`store_identity`), so
            // "the password never appears in the string that is hashed" has to
            // hold of it as well as of the echo.
            let identity = store_dsn_identity(raw);
            assert_no_secret("the identity", raw, &identity);
            assert!(
                identity.starts_with(UNACCOUNTED_IDENTITY_PREFIX),
                "a shape no parser accounts for gets a digest, not a quotation: \
                 {raw} -> {identity}"
            );
        }
    }

    /// The half of the promise that is not about safety: an operator who
    /// mistyped a port still has to be able to see the port they mistyped, or
    /// there was never any reason to quote the DSN in the message at all.
    #[test]
    fn a_recognised_shape_still_shows_the_operator_the_typo() {
        // The exact transcript from the round-2 review, unchanged by rounds 3
        // and 4.
        assert_eq!(
            store_dsn_echo("postgres://app:S3cretHunter@127.0.0.1:70000/lambo"),
            "postgres://app@127.0.0.1:70000/lambo"
        );
        assert_eq!(
            store_dsn_echo("postgres://app:S3cretHunter@[::1]:notaport/lambo"),
            "postgres://app@[::1]:notaport/lambo"
        );
        assert_eq!(
            store_dsn_echo("postgre://app:S3cretHunter@db.internal:26257/lambo"),
            "postgre://app@db.internal:26257/lambo"
        );
        assert_eq!(
            store_dsn_echo("postgresql://app:S3cretHunter@127.0.0.1:99999/lambo?sslmode=require"),
            "postgresql://app@127.0.0.1:99999/lambo?sslmode=require"
        );

        // A query that mentions a password loses the query, not the authority:
        // the mistyped port is still legible.
        assert_eq!(
            store_dsn_echo("postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter"),
            "postgres://app@127.0.0.1:70000/lambo?<query redacted>"
        );
        assert_eq!(
            store_dsn_echo("postgres://127.0.0.1:70000/lambo?password=S3cretHunter"),
            "postgres://127.0.0.1:70000/lambo?<query redacted>"
        );

        // A clean libpq keyword string keeps every allowlisted token but the
        // secret, even when a bad port stopped it parsing.
        assert_eq!(
            store_dsn_echo("host=127.0.0.1 port=70000 dbname=lambo user=app password=S3cretHunter"),
            "host=127.0.0.1 port=70000 dbname=lambo user=app"
        );

        // The last-`@` rule the round-3 review verified: an `@` in the path
        // over-redacts rather than mis-splitting, which is the safe direction.
        assert_eq!(
            store_dsn_echo("postgres://app:S3cretHunter@127.0.0.1:70000/lam@bo"),
            "postgres://app@bo"
        );

        // Two credentials for one unparseable spelling are one *echo* — the
        // operator sees the same thing either way, which is the point of
        // stripping. They are no longer one *identity*: see
        // `two_unparseable_spellings_are_two_identities` for why that changed
        // and what it costs.
        assert_eq!(
            store_dsn_echo("postgres://app:one@127.0.0.1:70000/lambo"),
            store_dsn_echo("postgres://app:two@127.0.0.1:70000/lambo")
        );
    }

    /// B-E2E-R4-1: the kv branch is an allowlist over *keys*, not a blacklist
    /// over the substring "password".
    ///
    /// Round 3 kept every token whose key merely lacked that substring, so a
    /// letter-transposition (`passwrod`), either of the two abbreviations an
    /// operator types by reflex (`pwd`, `pass`), a non-libpq key (`secret_pw`)
    /// and libpq's own second secret keyword (`sslpassword`) all printed
    /// verbatim on the line that says "(passwords stripped)".
    ///
    /// To watch this go red, put the blacklist back:
    ///
    /// ```ignore
    /// fn is_echoable_libpq_key(key: &str) -> bool { !mentions_password(key) }
    /// ```
    #[test]
    fn a_libpq_key_is_echoed_only_if_it_is_on_the_list() {
        for key in [
            "passwrod",
            "pwd",
            "pass",
            "secret_pw",
            "sslpassword",
            "passfile",
        ] {
            let raw = format!("host=127.0.0.1 port=70000 user=app {key}=S3cretHunter");
            let echo = store_dsn_echo(&raw);
            assert_no_secret("the kv echo", &raw, &echo);
            // The mistyped port survives, which is the only reason to quote it.
            assert_eq!(echo, "host=127.0.0.1 port=70000 user=app", "key {key}");
        }

        // The allowlisted keys are all still echoed, so the diagnostic value
        // the whole branch exists for is not quietly gone.
        assert_eq!(
            store_dsn_echo(
                "host=h port=70000 dbname=d user=u sslmode=require connect_timeout=3 \
                 application_name=lambo"
            ),
            "host=h port=70000 dbname=d user=u sslmode=require connect_timeout=3 \
             application_name=lambo"
        );

        // `options` is off the list deliberately: libpq forwards it to the
        // backend verbatim, so it can hold anything an operator put there.
        assert_eq!(
            store_dsn_echo("host=h port=70000 options=-csearch_path=x"),
            "host=h port=70000"
        );
    }

    /// B-E2E-R4-3: a password with an unencoded `/` or `?` in it does not reach
    /// the identity, the echo, or the socket-path hash.
    ///
    /// `split_authority` stops at the first `/` or `?` (RFC 3986 says the
    /// authority ends there), so an operator who typed their password raw
    /// instead of percent-encoding it had it re-read as a host or a database
    /// name and printed verbatim. sqlx rejects two of these three outright, so
    /// the guards agree with the driver rather than inventing a rule.
    ///
    /// To watch this go red, drop either guard: the `path.contains(':')` check
    /// in `parse_postgres_url`, or `split_host_port`'s `Some(_) => None` arm.
    #[test]
    fn an_unencoded_slash_in_a_password_does_not_reach_the_identity() {
        for raw in [
            "postgres://ap/p:S3cretHunter@h:70000/db",
            "postgres://app:S3cret/Hunter@h/db",
            "postgres://app:S3cret?Hunter@h/db",
        ] {
            assert!(
                parse_postgres_url(raw).is_none(),
                "a userinfo cut in half by an unencoded delimiter must not parse: {raw}"
            );
            assert_no_secret("the echo", raw, &store_dsn_echo(raw));
            assert_no_secret("the identity", raw, &store_dsn_identity(raw));
        }

        // The correct spelling is unaffected, which is what makes the guards a
        // guard and not a ban: `%2F` is a slash in a password and parses.
        assert_eq!(
            store_dsn_identity("postgres://app:S3cret%2FHunter@h/db"),
            "postgres://app@h:5432/db"
        );
        assert_eq!(
            store_dsn_echo("postgres://app:S3cret%2FHunter@h/db"),
            "postgres://app@h:5432/db"
        );
        // And so is `%3A` for a colon in a database name.
        assert_eq!(
            store_dsn_identity("postgres://app@h/lam%3Abo"),
            "postgres://app@h:5432/lam:bo"
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
            // nothing survives the key allowlist, so there is nothing to say
            "password=S3cretHunter",
            "sslpassword=S3cretHunter",
        ] {
            assert_eq!(
                store_dsn_echo(raw),
                UNPARSEABLE_DSN,
                "no branch accounts for {raw}, so nothing from it may be echoed"
            );
        }
    }

    /// B-E2E-R4-2: two spellings this module cannot parse are two identities.
    ///
    /// Round 3 gave them all the constant `<unparseable dsn>` and defended the
    /// collapse with "both are strings no driver will dial". They dial. The
    /// pair below is the one round 4 constructed: both sides collapse onto the
    /// placeholder (the literal "password" in the database name fails
    /// `is_safe_to_echo`, and the empty port defeats `split_host_port`), and
    /// `sqlx::postgres::PgConnectOptions::from_str` resolves them to two
    /// different real hosts holding two different real databases. Under round 3
    /// `overlay_env` found the two sides equal, skipped its refusal, and took
    /// the environment's — E2E-F2, the workstream's original P1, reopened.
    ///
    /// [`sqlx_dials_what_this_module_cannot_parse`] is the measurement; this is
    /// the consequence. To watch it go red, make `unaccounted_identity` return
    /// `UNPARSEABLE_DSN.to_string()`.
    #[test]
    fn two_unparseable_spellings_are_two_identities() {
        let pairs = [
            // Round 4's constructed E2E-F2 reopening, dialable both sides.
            (
                "postgres://app@host-a:/db_password_a",
                "postgres://app@host-b:/db_password_b",
            ),
            // Round 3's own pinned collapse example, which it asserted was
            // undialable. sqlx dials it.
            ("app:one@host-a:26257/db-a", "app:two@host-b:26257/db-b"),
            // The scheme-less class generally.
            (
                "app:S3cretHunter@host-a:26257/lambo",
                "app:S3cretHunter@host-b:26257/lambo",
            ),
        ];
        for (a, b) in pairs {
            assert_eq!(
                store_dsn_echo(a),
                UNPARSEABLE_DSN,
                "this pair is only interesting while both sides are withheld"
            );
            assert_eq!(store_dsn_echo(b), UNPARSEABLE_DSN);
            assert_ne!(
                store_dsn_identity(a),
                store_dsn_identity(b),
                "two spellings that dial different databases must not be one \
                 identity just because neither could be quoted: {a} vs {b}"
            );
        }

        // The price, pinned so that paying it stays deliberate. Two spellings
        // that differ *only* by a password are one database and were one
        // identity before this round; a digest of the raw input cannot know
        // that, so they are now two. The consequence is a refusal naming both
        // sides — loud, and fixed by one edit — where the alternative was the
        // environment silently outranking the file. Over-splitting is the safe
        // direction to be wrong in, the same way over-redacting is.
        assert_ne!(
            store_dsn_identity("postgre://app@h:26257/db"),
            store_dsn_identity("postgre://app:S3cretHunter@h:26257/db"),
        );
        // It costs nothing on the parsed path, which is where the documented
        // "the file names the database, the environment supplies the password"
        // pattern actually lives.
        assert_eq!(
            store_dsn_identity("postgres://app@h:26257/db"),
            store_dsn_identity("postgres://app:S3cretHunter@h:26257/db"),
        );
    }

    /// The digest is never handed to a terminal, and the placeholder is never
    /// handed to a comparison. Two constants, two jobs, no overlap.
    #[test]
    fn the_echo_and_the_identity_do_not_borrow_each_others_answers() {
        let unparseable = "app:S3cretHunter@host-a:26257/lambo";
        assert_eq!(store_dsn_echo(unparseable), UNPARSEABLE_DSN);
        assert!(!store_dsn_echo(unparseable).contains(UNACCOUNTED_IDENTITY_PREFIX));
        assert_ne!(store_dsn_identity(unparseable), UNPARSEABLE_DSN);

        // Absent stays absent on both halves: a `path`-shaped store has no DSN
        // and must not acquire a digest of the empty string as an identity.
        assert_eq!(store_dsn_identity(""), "");
        assert_eq!(store_dsn_echo(""), "");
        assert_eq!(store_dsn_identity("   "), "");
        assert_eq!(store_dsn_echo("   "), "");

        // The identity is stable: the same spelling twice is the same digest,
        // or `overlay_env` would refuse a config that agrees with itself.
        assert_eq!(
            store_dsn_identity(unparseable),
            store_dsn_identity(unparseable)
        );
        // And whitespace around it is not part of the spelling.
        assert_eq!(
            store_dsn_identity(unparseable),
            store_dsn_identity(&format!("  {unparseable}  "))
        );

        // The note that keeps two withheld quotes from reading as a bug fires
        // exactly when one of them is withheld.
        assert!(withheld_note("postgres://a@h:5432/d", "postgres://b@h:5432/d").is_empty());
        assert!(!withheld_note(UNPARSEABLE_DSN, "postgres://b@h:5432/d").is_empty());
        assert!(!withheld_note(UNPARSEABLE_DSN, UNPARSEABLE_DSN).is_empty());
    }

    /// A parsed identity is safe to print without a keyword gate, because it is
    /// four fields and none of them is the password — but it still reaches a
    /// terminal, and a percent-encoded escape in a database name arrives
    /// intact. That clause holds on every path.
    #[test]
    fn a_parsed_echo_still_may_not_carry_a_control_character() {
        let raw = "postgres://app@h:5432/lam%1b%5b2Jbo";
        // It parses, and the identity keeps the operator's database name.
        assert!(parse_postgres_url(raw).is_some());
        assert!(store_dsn_identity(raw).contains('\u{1b}'));
        // The echo does not.
        assert_eq!(store_dsn_echo(raw), UNPARSEABLE_DSN);
    }

    /// The parsed paths were already safe; pinned here so both halves of the
    /// promise sit in one place.
    #[test]
    fn a_parseable_dsn_has_its_password_stripped() {
        assert_eq!(
            store_dsn_identity("postgres://app:S3cretHunter@127.0.0.1:5432/lambo"),
            "postgres://app@127.0.0.1:5432/lambo"
        );
        assert_eq!(
            store_dsn_identity(
                "host=127.0.0.1 port=5432 dbname=lambo user=app password=S3cretHunter"
            ),
            "postgres://app@127.0.0.1:5432/lambo"
        );
        assert_eq!(
            store_dsn_echo("postgres://app:S3cretHunter@127.0.0.1:5432/lambo"),
            "postgres://app@127.0.0.1:5432/lambo"
        );
    }

    /// B-E2E-R4-2, the measurement round 3 asserted instead of running.
    ///
    /// Round 3's report, commit message and doc comment all justified collapsing
    /// unrecognised spellings onto one identity with "both are strings no driver
    /// will dial — anything sqlx can actually connect to parses through
    /// `parse_postgres_url` and never reaches this function". This test is that
    /// claim, executed. It fails, which is why the collapse is gone.
    ///
    /// `PgConnectOptions::from_str` is the real dial path, not a stand-in:
    /// `store::pg` reaches the network through `dsn.parse::<PgConnectOptions>()`
    /// (`connect_options`, `src/store/pg/mod.rs`). sqlx validates no scheme at
    /// all — it hands the string to the `url` crate and reads components off
    /// whatever comes back — which is the mechanism behind every row here.
    ///
    /// Feature-gated because it needs the driver, so it compiles under
    /// `store-postgres` and `store-cockroach` and not under `store-sqlite`
    /// alone.
    #[cfg(any(feature = "store-postgres", feature = "store-cockroach"))]
    #[test]
    fn sqlx_dials_what_this_module_cannot_parse() {
        use std::str::FromStr;
        let dial = |raw: &str| sqlx::postgres::PgConnectOptions::from_str(raw);

        // Round 4's constructed pair: two different real hosts, two different
        // real databases, and round 3 gave both the same identity.
        let a = dial("postgres://app@host-a:/db_password_a").expect("sqlx accepts an empty port");
        let b = dial("postgres://app@host-b:/db_password_b").expect("sqlx accepts an empty port");
        assert_eq!(
            (a.get_host(), a.get_database()),
            ("host-a", Some("db_password_a"))
        );
        assert_eq!(
            (b.get_host(), b.get_database()),
            ("host-b", Some("db_password_b"))
        );
        assert!(parse_postgres_url("postgres://app@host-a:/db_password_a").is_none());
        assert_ne!(
            store_dsn_identity("postgres://app@host-a:/db_password_a"),
            store_dsn_identity("postgres://app@host-b:/db_password_b"),
        );

        // Round 3's own pinned collapse example. It claimed no driver would
        // dial it; sqlx reads `app` as the scheme, defaults the host, and takes
        // the rest as a database name.
        let c = dial("app:one@host-a:26257/db-a").expect("sqlx validates no scheme");
        assert_eq!(c.get_database(), Some("one@host-a:26257/db-a"));

        // A misspelled scheme is dialed at the host and port it names.
        let d = dial("postgre://app@h:26257/db").expect("sqlx validates no scheme");
        assert_eq!((d.get_host(), d.get_port()), ("h", 26257));

        // The two guards added for B-E2E-R4-3 agree with the driver: sqlx
        // refuses these for the same reason we now do.
        assert!(dial("postgres://app:S3cret/Hunter@h/db").is_err());
        assert!(dial("postgres://app:S3cret?Hunter@h/db").is_err());
        // And the shapes R2-5 filed are refused by sqlx too, which is why
        // "would it dial" was never the right question on a refusal path.
        assert!(dial("postgres://app:S3cretHunter@h:70000/db").is_err());
    }
}
