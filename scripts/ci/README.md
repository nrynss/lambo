# CI and release checks

`ci.yml` runs a lightweight changes classifier on every PR and main push.
Rust inputs start the full feature matrix; mirrored reference pages start
the documentation drift check. Unknown or unavailable diff bases run both.
`workflow_dispatch` always runs full validation.

The `full-ci` job succeeds only after all enabled Rust validation jobs pass.
It is skipped on documentation-only runs. Release preflight requires both
the newest matching main CI run and its current attempt's `full-ci` job to
succeed for the exact tagged commit. A green PR or documentation-only run
does not qualify. Builds, checksums and publishing follow; release does not
rerun tests.

If a release commit has no full CI run, start one and wait for it to pass:

```sh
gh workflow run ci.yml --ref main
```

Tag the validated commit as `v<package.version>` to publish. A manual release
workflow run is a diagnostic dry run: it bypasses the green-CI gate, builds
assets and uploads workflow artifacts, and cannot publish.

When adding an embedded file or test fixture outside the existing input
trees, update `classify_changes.py` and its independent input-boundary tests.
Keep the classifier's mirror list aligned with `scripts/docs/check-mirror-drift.sh`.

Validate workflow changes locally with:

```sh
python3 -m unittest discover -s scripts/ci -p 'test_*.py'
actionlint -ignore 'constant expression "false" in condition' .github/workflows/*.yml
bash scripts/docs/check-mirror-drift.sh
```

The narrow actionlint exception preserves the deliberately disabled
CockroachDB live job. It does not enable that job or hide other findings.
