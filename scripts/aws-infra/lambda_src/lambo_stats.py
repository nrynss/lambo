"""`Lambda-LamboStats-API` - a public, read-only stats endpoint over the live
CockroachDB session (plan §2, §11).

Runs **outside the VPC**. It reads CockroachDB Cloud, which lives on the public
internet; a VPC-attached Lambda would need a NAT gateway to reach it, and it has
no reason to touch RDS. See plan §2 and §7: the argument is architectural, not
financial.

Read-only by construction: every statement here is a SELECT, the connection is
opened with `autocommit` left off and never committed, and there is no code path
that writes. This mirrors what `lambo serve-web` exposes at `/api/stats`
(`WebStats` in src/cli/serve_web.rs), computed independently over SQL so the
endpoint stays up even when the exhibit instance is being rebuilt.

The DSN is resolved from Secrets Manager at cold start and cached for the life of
the execution environment. It is never logged, never returned in a response, and
never placed in an environment variable - only the secret's *id* is.

Driver: `pg8000`, vendored into the deployment zip by `provision_app_data.py`.
It is pure Python, so it needs no compiled wheel and works unchanged on arm64.
`psycopg2` would have needed a per-architecture binary layer.
"""

from __future__ import annotations

import json
import os
import ssl
from urllib.parse import parse_qs, unquote, urlparse

import boto3
import pg8000.dbapi

_SECRET_ID = os.environ["LAMBO_DSN_SECRET_ID"]
_SESSION_ID = os.environ.get("LAMBO_SESSION", "")

_conn = None  # reused across invocations in a warm environment


def _dsn() -> str:
    """Fetch the DSN. Called once per execution environment, never logged."""
    client = boto3.client("secretsmanager")
    resp = client.get_secret_value(SecretId=_SECRET_ID)
    value = resp.get("SecretString")
    if not value:
        raise RuntimeError(
            f"secret {_SECRET_ID} has no string value; set it with "
            "`aws secretsmanager put-secret-value`"
        )
    return value.strip()


def _connect():
    """Open a verified-TLS connection to CockroachDB Cloud.

    `sslmode=verify-full&sslrootcert=system` (AGENTS.md) means: verify the chain
    against the OS trust store and check the hostname. `ssl.create_default_context()`
    is exactly that, so the DSN's ssl parameters are honoured rather than ignored.
    Anything weaker is refused outright rather than silently downgraded.
    """
    url = urlparse(_dsn())
    if url.scheme not in ("postgres", "postgresql", "cockroachdb"):
        raise RuntimeError(f"unsupported DSN scheme {url.scheme!r}")

    params = parse_qs(url.query)
    sslmode = (params.get("sslmode") or ["verify-full"])[0]
    if sslmode not in ("verify-full", "verify-ca", "require"):
        raise RuntimeError(
            f"refusing to connect with sslmode={sslmode!r}; this endpoint reads a "
            "database over the public internet and requires TLS"
        )
    context = ssl.create_default_context()
    if sslmode != "verify-full":
        context.check_hostname = False

    database = url.path.lstrip("/") or "defaultdb"
    user = unquote(url.username or "")

    # Older CockroachDB Cloud DSNs route to the cluster with `options=--cluster=<id>`
    # rather than by SNI. pg8000 has no `options` parameter, but Cockroach accepts
    # the documented equivalent: prefix the username with `<cluster>.`. Translate
    # it rather than dropping it, because dropping it would connect to the wrong
    # place with a confusing authentication error.
    options = (params.get("options") or [""])[0]
    for token in options.split():
        if token.startswith("--cluster=") and "." not in user:
            user = f"{token.split('=', 1)[1]}.{user}"

    return pg8000.dbapi.connect(
        host=url.hostname,
        port=url.port or 26257,
        database=database,
        user=user,
        password=unquote(url.password or ""),
        ssl_context=context,
        timeout=10,
    )


def _cursor():
    global _conn
    if _conn is not None:
        try:
            with _conn.cursor() as cur:
                cur.execute("SELECT 1")
            return _conn.cursor()
        except Exception:  # noqa: BLE001 - stale connection; drop and redial
            try:
                _conn.close()
            except Exception:  # noqa: BLE001
                pass
            _conn = None
    _conn = _connect()
    return _conn.cursor()


def _scalar(cur, sql: str, params: tuple) -> int:
    cur.execute(sql, params)
    row = cur.fetchone()
    return int(row[0]) if row and row[0] is not None else 0


def _stats(session_id: str) -> dict:
    cur = _cursor()
    try:
        stats = {
            "session": session_id,
            "concepts": _scalar(cur, "SELECT count(*) FROM concepts WHERE session_id = %s", (session_id,)),
            "canonical": _scalar(
                cur,
                "SELECT count(*) FROM concepts WHERE session_id = %s "
                "AND canonization_status = 'Canonical'",
                (session_id,),
            ),
            "edges": _scalar(cur, "SELECT count(*) FROM edges WHERE session_id = %s", (session_id,)),
            "interactions": _scalar(
                cur, "SELECT count(*) FROM interactions WHERE session_id = %s", (session_id,)
            ),
            "canonization_events": _scalar(
                cur, "SELECT count(*) FROM canonization_events WHERE session_id = %s", (session_id,)
            ),
        }
        # The pillars, highest blast radius first. This is the substance of the
        # exhibit: the nodes whose deletion the demo intercepts.
        cur.execute(
            "SELECT content, canonization_status, coalesce(blast_radius, 0) "
            "FROM concepts WHERE session_id = %s AND canonization_status = 'Canonical' "
            "ORDER BY coalesce(blast_radius, 0) DESC, content ASC LIMIT 10",
            (session_id,),
        )
        stats["pillars"] = [
            {"content": r[0], "status": r[1], "blast_radius": int(r[2])} for r in cur.fetchall()
        ]
        cur.execute(
            "SELECT occurred_at, node_id, from_status, to_status, coalesce(blast_radius, 0) "
            "FROM canonization_events WHERE session_id = %s "
            "ORDER BY occurred_at DESC LIMIT 20",
            (session_id,),
        )
        stats["recent_canonization_events"] = [
            {
                "occurred_at": r[0].isoformat() if r[0] else None,
                "node_id": str(r[1]),
                "from_status": r[2],
                "to_status": r[3],
                "blast_radius": int(r[4]),
            }
            for r in cur.fetchall()
        ]
        return stats
    finally:
        cur.close()


def handler(event, _context):
    """Function URL entry point. Returns JSON; never echoes the DSN."""
    session_id = _SESSION_ID
    query = (event or {}).get("queryStringParameters") or {}
    if query.get("session"):
        session_id = query["session"]

    headers = {
        "content-type": "application/json",
        # Short cache: judges refresh this, and the numbers move slowly.
        "cache-control": "public, max-age=10",
    }

    if not session_id:
        return {
            "statusCode": 400,
            "headers": headers,
            "body": json.dumps(
                {"error": "no session; set LAMBO_SESSION or pass ?session=<id>"}
            ),
        }

    try:
        body = _stats(session_id)
    except Exception as exc:  # noqa: BLE001
        # Deliberately generic: the exception text from a driver can contain the
        # host and user from the DSN, and this response is public.
        print(f"stats query failed: {type(exc).__name__}")
        return {
            "statusCode": 503,
            "headers": headers,
            "body": json.dumps({"error": "stats unavailable", "session": session_id}),
        }

    return {"statusCode": 200, "headers": headers, "body": json.dumps(body)}
