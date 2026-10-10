# #92 — Content-Security-Policy and nosniff on the portal

`lambo serve-web` now stamps two response headers. The owner decision of
2026-10-10 supersedes the earlier dogfood recommendation (id `6dcffb4c`)
to put the policy in a `<meta http-equiv>` tag. `frame-ancestors` and
`X-Content-Type-Options` are not delivered by a meta policy, so the
headers are real response headers. No meta CSP tag was added.

## Policy

On HTML page responses only (`GET` and `HEAD` of `/` and of
`/s/{session}/`, and any other route that serves `index.html`):

```
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'
```

Deviation from the draft, which had `img-src 'self' data:`: the page has
no `<img>`, `web/app.css` has no `url(`, and nothing uses a `data:` URL,
so `data:` is not in the policy. `form-action 'self'` stays because
`web/index.html` has `<form id="session-picker">` with no `action` (a
native submit targets the current document). The script calls
`preventDefault` and `window.location.assign`, which `form-action` does
not govern. `font-src 'self'` stays as specified: there are no webfonts,
and `'self'` does not allow a third-party font. `app.js` assigns
`element.style.*` and `element.onclick =` from the external script. Those
are allowed by `style-src 'self'` and `script-src 'self'` without
`'unsafe-inline'`. The policy has no `'unsafe-inline'` and no
`'unsafe-eval'`.

`X-Content-Type-Options: nosniff` is on every response the router
answers: pages, `/app.js`, `/app.css`, `/healthz`, `/api/*`, `308`, `404`,
`401`, `403`, `405`, and the `400` / `502` / `503` answers the handlers
already build. Hyper's own replies to a request it cannot parse (a
malformed request line, a `431` for oversized headers) never enter the
axum router and carry neither header; they are not intercepted.
The policy is not set on JSON, assets, healthz, redirects, or errors. It
is keyed off the response `Content-Type` being `text/html` (the index
handler sets `text/html; charset=utf-8`). `HEAD` of `/` and of
`/s/{session}/` keeps that content-type, because axum runs the `GET`
handler and drops the body; no separate HEAD detection was needed.

## Layer order

`security_headers` (`src/cli/serve_web/headers.rs`, axum
`middleware::from_fn`, no tower-http, `Cargo.toml` and `Cargo.lock`
unchanged) is the outermost layer, added after `host_guard`:

```
fallback service (routes, with the gate)
  .layer(resolve_session)
  .layer(host_guard)
  .layer(security_headers)
```

Axum runs the last layer first on the way in and last on the way out.
`host_guard` returns its `403` without calling onward, and
`resolve_session` returns `401`, the uniform `404`, and the `308` the
same way. An inner layer never sees those responses. `headers.insert`
with `HeaderValue::from_static` sets each header once. Nobody was moved
to `route_layer`.

## Byte identity

One SQLite session (`byteid`, fixture embedder), same store, `lambo-main`
on `127.0.0.1:18792` and this branch on `127.0.0.1:18793`. Compared with
one raw HTTP/1.1 client (not curl): `GET` and `HEAD` of `/`, `/app.js`,
`/app.css`, `/healthz`, `/api/session`, `/api/stats`, and `/no/such`;
`POST /` (`Content-Length: 0`, status 405); `GET /` with
`Host: rebind.example` (status 403). Sixteen responses. After stripping
`Date`, `Content-Security-Policy`, and `X-Content-Type-Options`, status
line, remaining headers, and body matched in every case
(`mismatches=0`). The branch sent the exact policy only on `GET /` and
`HEAD /`, and `nosniff` on all sixteen. Main sent neither header. Both
servers were stopped.

## Browser

Google Chrome 155 headless (`--dump-dom`, virtual time budget 12s) against
this branch on `127.0.0.1:18794` and `127.0.0.1:18795`, not 7700 or 7710.
No browser MCP was attached.

- Listing on, two sessions (`shown`, `empty`, `[web] list_sessions = true`):
  the picker is visible and offers `shown`, `empty`, and "Other session…".
- One session (`solo`, listing off): the picker stays `hidden`. The session
  name and the concept "plain concept" render.
- Deep link `/s/shown/` shows that session and the same picker.
- Deep link `/s/empty/` says "No memory in this session yet".
- A concept whose text is `<img src=x onerror=alert(1)>` is a tree button
  whose text is that string escaped (`&lt;img …&gt;`). The dump has no
  `<img` element. The tree row's `style="padding-left: 8px;"` comes from
  the external script's CSSOM write.

Stderr has headless display and GPU noise (`CVDisplayLink`,
`SharedImageManager`) and no Content-Security-Policy violation.

## Caddy

`launch_exhibit_ec2.py --dry-run` (no AWS calls) with `--session byteid
--embedder fixture`. Hostname `portal.example.com`:

```
portal.example.com {
    encode zstd gzip
    reverse_proxy 127.0.0.1:7710
}
```

Self-signed:

```
:443 {
    tls internal
    encode zstd gzip
    reverse_proxy 127.0.0.1:7710 {
        header_up Host {upstream_hostport}
    }
}
```

`header_up` sets the request `Host`, not a response header. The file does
not set or strip response headers. Caddy's `reverse_proxy` passes the
upstream headers through, so it neither strips nor duplicates the policy
or `nosniff`. The template was not changed.

## Mutation checks

`pages_carry_the_exact_csp_and_every_response_is_nosniff` is green on the
shipped code. Each mutation was a copy of the file, a one-line edit, the
test, then the copy restored (not `git checkout`):

1. Removing the `nosniff` insert fails on the pre-routing Host `403`
   (`left: []`, `right: ["nosniff"]`).
2. Removing the CSP insert fails the exact-value assertion on `GET /`.
3. Moving `.layer(security_headers)` inside `host_guard` fails the same
   Host `403` `nosniff` assertion. The page assertions are not what fails.
