//! DSN, TLS, IAM and environment configuration.

use super::*;

/// `LAMBO_POSTGRES_IAM` is a Cloud SQL opt-in, and a binary carrying both adapters
/// (which `ship` does) must not hand a Cloud SQL token to a Cockroach cluster because
/// one variable was exported for the other store. Cockroach ignores it: with the
/// variable set and NO credentials anywhere, this store still builds its ordinary
/// pool, where the Postgres dialect refuses (see
/// `iam_without_credentials_fails_closed_naming_the_variables`).
#[cfg(feature = "store-postgres")]
#[tokio::test]
async fn cockroach_ignores_the_cloud_sql_iam_opt_in() {
    const { assert!(!CockroachDialect::SUPPORTS_CLOUD_SQL_IAM_AUTH) };
    let store = {
        let env = crate::test_util::env_lock();
        env.set("LAMBO_POSTGRES_IAM", "1");
        env.remove("GCP_LAMBO_CREDENTIALS");
        env.remove("GOOGLE_APPLICATION_CREDENTIALS");
        CockroachStore::new(StoreConfig {
            kind: crate::store::StoreKind::Cockroach,
            dsn: Some("postgresql://u@127.0.0.1:1/lambo?sslmode=disable".into()),
            path: None,
            vector_dim: None,
        })
        .expect("construct")
    };
    store
        .pool()
        .await
        .expect("cockroach must not take the Cloud SQL IAM path");
}

/// E2E-F2: the Cockroach dialect's DSN variable and the one configuration
/// resolution reads for `kind = "cockroach"` must be one string.
#[test]
fn dsn_env_named_in_errors_is_the_one_config_reads() {
    assert_eq!(CockroachDialect::DSN_ENV, crate::store::COCKROACH_DSN_ENV,);
    assert_eq!(
        crate::store::StoreKind::Cockroach.dsn_env(),
        Some(CockroachDialect::DSN_ENV),
    );
}

/// T7.4: the ANN accuracy dial parses and fails closed. A tuning knob that
/// is silently ignored on a typo is worse than no knob — the operator
/// believes accuracy was raised when it was not.
#[test]
fn vector_beam_size_env_parses_and_fails_closed() {
    let env = crate::test_util::env_lock();

    env.remove(VECTOR_BEAM_SIZE_ENV);
    assert_eq!(
        vector_beam_size_from_env().unwrap(),
        None,
        "the parser reports absence; the DEFAULT is applied at the call site"
    );
    // Pin the measured default (adve-review MAJOR-1). 32 is CockroachDB's
    // default and measured ~6-7% worse on recall; 256 measured WORSE than
    // 64. If this constant changes, the measurement in its doc must be
    // redone — it is evidence-backed, not a taste call.
    assert_eq!(DEFAULT_VECTOR_BEAM_SIZE, 64);
    assert!(
        (VECTOR_BEAM_SIZE_MIN..=VECTOR_BEAM_SIZE_MAX).contains(&DEFAULT_VECTOR_BEAM_SIZE),
        "default must satisfy the server's own bounds"
    );

    // Exported-but-blank behaves as absent (same convention as LAMBO_STORE).
    env.set(VECTOR_BEAM_SIZE_ENV, "");
    assert_eq!(vector_beam_size_from_env().unwrap(), None);
    env.set(VECTOR_BEAM_SIZE_ENV, "   ");
    assert_eq!(vector_beam_size_from_env().unwrap(), None);

    env.set(VECTOR_BEAM_SIZE_ENV, "128");
    assert_eq!(vector_beam_size_from_env().unwrap(), Some(128));
    // Server-enforced bounds, verified live 2026-08-13.
    env.set(VECTOR_BEAM_SIZE_ENV, "1");
    assert_eq!(vector_beam_size_from_env().unwrap(), Some(1));
    env.set(VECTOR_BEAM_SIZE_ENV, "2048");
    assert_eq!(vector_beam_size_from_env().unwrap(), Some(2048));

    for bad in ["0", "2049", "-1", "64.5", "many", "1e3"] {
        env.set(VECTOR_BEAM_SIZE_ENV, bad);
        assert!(
            vector_beam_size_from_env().is_err(),
            "{bad:?} must be rejected at pool construction, not silently dropped"
        );
    }
}

#[test]
fn dsn_for_rustls_rewrites_sslrootcert_system() {
    let out = dsn_for_rustls("postgresql://u:p@h:26257/db?sslmode=verify-full&sslrootcert=system");
    assert!(!out.contains("sslrootcert=system"), "{out}");
    let has_bundle = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
        "/etc/ssl/ca-bundle.pem",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).is_file());
    if has_bundle {
        assert!(
            out.contains("sslrootcert=/") && !out.contains("system"),
            "{out}"
        );
    } else {
        assert!(!out.contains("sslmode=verify-full"), "downgraded: {out}");
    }
    // Dangling separators cleaned.
    assert!(
        !out.contains("?&") && !out.ends_with('&') && !out.ends_with('?'),
        "{out}"
    );
    // Untouched DSN passes through unchanged.
    let plain = "postgresql://u:p@h:26257/db?sslmode=require";
    assert_eq!(dsn_for_rustls(plain), plain);
}
