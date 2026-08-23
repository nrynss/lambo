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
//! form is safe to print in an error message.

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
///   it contains `=`. Anything else is returned with a `password=` token
///   stripped, otherwise verbatim.
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
    strip_libpq_password_token(trimmed)
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

fn strip_libpq_password_token(raw: &str) -> String {
    raw.split_whitespace()
        .filter(|tok| {
            let key = tok.split('=').next().unwrap_or("").to_ascii_lowercase();
            key != "password"
        })
        .collect::<Vec<_>>()
        .join(" ")
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
