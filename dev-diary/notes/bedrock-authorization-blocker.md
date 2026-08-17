# Bedrock authorization blocker — wait 24h then retry

**Account:** `<AWS_ACCOUNT_ID>`  
**Status:** blocked at Bedrock **service backend** (`authorizationStatus: NOT_AUTHORIZED`)  
**Lambo impact:** T0.4 Bedrock path blocked. **Hybrid embeddings proceed via portable BGE-M3
(HF + llama.cpp)** — see [`embeddings-portable.md`](embeddings-portable.md). Bedrock Titan
remains optional swap-in when authorized.  
**Plan:** wait **24 hours** from first non-Bedrock activity (EC2 launch ~2026-08-10 ~12:00 UTC), then retest Bedrock; if still blocked, escalate support with the follow-up text below. Demo does **not** wait on this for hybrid.

---

## Root cause (AWS guidance)

`authorizationStatus: NOT_AUTHORIZED` from `get-foundation-model-availability` is an **account-level** authorization status, separate from:

- IAM permissions  
- Marketplace entitlements  
- Regional availability  

On this account, the three prerequisite fields are all **AVAILABLE**:

| Field | Value |
|-------|--------|
| `agreementAvailability` | AVAILABLE |
| `entitlementAvailability` | AVAILABLE |
| `regionAvailability` | AVAILABLE |
| **`authorizationStatus`** | **NOT_AUTHORIZED** |

For **Amazon first-party** models (e.g. Titan Text Embeddings V2), no Marketplace subscription is required. `NOT_AUTHORIZED` means the account has not been cleared for Bedrock invocation at the **service backend**. This often affects newly created / recently activated accounts.

`put-use-case-for-model-access` failing with “account not authorized” further confirms this is **not** fixable by console/API config alone.

---

## What already failed (do not re-derive)

| Check | Result |
|-------|--------|
| Model catalog | Titan V2 visible; playground **disabled** |
| Model access page | **Retired** (auto-enable on first invoke is the intended path) |
| Payment method | Configured from the start |
| Root `aws login` | STS works |
| Bedrock API key (Bearer, ap-south-2) | Auth accepted → still `Operation not allowed` |
| Root `InvokeModel` us-east-1 | `ValidationException: Operation not allowed` |
| Root `InvokeModel` ap-south-2 | Same |
| Agreement offers (Titan) | `Agreement not supported for this model` (expected for Amazon FM) |
| AWS Organizations / SCP | Not in an org |
| Free-tier EC2 | `t3.micro` `i-08fe9678f080dc575` launched us-east-1a; Lambo built+tested on box |
| IAM user `lambo-bedrock-test` | Created + Bedrock inline policy; no permission boundary |
| IAM access keys for that user | `InvalidClientTokenId` / invalid security token on STS (keys deleted after test) |
| Role `LamboBedrockTestRole` | Created; **root cannot AssumeRole** (`Roles may not be assumed by root accounts`) |

**Diagnostic resources left in account (no live access keys):**

- User: `arn:aws:iam::<AWS_ACCOUNT_ID>:user/lambo-bedrock-test`  
- Role: `arn:aws:iam::<AWS_ACCOUNT_ID>:role/LamboBedrockTestRole`  
- Inline policy name: `LamboBedrockInvoke` (InvokeModel, list/availability)

---

## Target model / region for retest

Prefer **one region only** (AWS: authorization is region-specific; avoid mixing):

- **Region:** `us-east-1`  
- **Model ID:** `amazon.titan-embed-text-v2:0`  
- **Body:** `{"inputText":"test","dimensions":1024,"normalize":true}`

After unlock, Lambo default/app region may still be `ap-south-2` for other work; embeddings can use `LAMBO_BEDROCK_REGION=us-east-1` if needed.

---

## After ~24 hours — retry checklist

```bash
# 1) Clean auth for control plane (prefer aws login, not expired bearer)
unset AWS_BEARER_TOKEN_BEDROCK
unset LAMBO_BEDROCK_API_KEY
aws login --region us-east-1   # if session expired
aws sts get-caller-identity

# 2) Availability — want AUTHORIZED
aws bedrock get-foundation-model-availability \
  --region us-east-1 \
  --model-id amazon.titan-embed-text-v2:0

# 3) Invoke
aws bedrock-runtime invoke-model \
  --region us-east-1 \
  --model-id amazon.titan-embed-text-v2:0 \
  --content-type application/json \
  --accept application/json \
  --body '{"inputText":"test","dimensions":1024,"normalize":true}' \
  --cli-binary-format raw-in-base64-out \
  /tmp/titan-out.json

python3 -c "import json;d=json.load(open('/tmp/titan-out.json'));print('dims',len(d['embedding']))"

# 4) Lambo spike (loads repo .env if present)
cd /path/to/lambo/spikes/bedrock-spike
LAMBO_BEDROCK_REGION=us-east-1 cargo run
```

**Success criteria:**

- `"authorizationStatus": "AUTHORIZED"`  
- Playground enabled in console (optional confirmation)  
- Invoke returns 1024-dim embedding  
- Spike prints `=== VERDICT: OK ===`

**If still `NOT_AUTHORIZED`:** escalate support with the follow-up body below (do not re-run endless console experiments).

---

## Optional: IAM user path (after keys work)

Root cannot assume roles. If long-lived IAM keys start working:

```bash
# create key for lambo-bedrock-test, export AWS_ACCESS_KEY_ID / SECRET
aws sts get-caller-identity
aws bedrock get-foundation-model-availability \
  --region us-east-1 \
  --model-id amazon.titan-embed-text-v2:0
# then InvokeModel as above
# delete access key when done
```

Or assume `LamboBedrockTestRole` **from the IAM user** (not root).

---

## Support case — original subject

```text
Bedrock foundation models blocked: authorizationStatus NOT_AUTHORIZED / playground disabled
```

## Support case — follow-up after failed self-service (paste if still blocked after 24h)

```text
Follow-up: completed all self-service troubleshooting from prior guidance.
Bedrock remains blocked after wait / retries.

Account: <AWS_ACCOUNT_ID>
Region under test: us-east-1 only (no region mixing)

1) get-foundation-model-availability (us-east-1, amazon.titan-embed-text-v2:0)
   agreementAvailability: AVAILABLE
   entitlementAvailability: AVAILABLE
   regionAvailability: AVAILABLE
   authorizationStatus: NOT_AUTHORIZED

2) Root InvokeModel (us-east-1):
   ValidationException: Operation not allowed

3) Organizations/SCP:
   AWSOrganizationsNotInUseException — account not in an org; no SCPs.

4) list-foundation-model-agreement-offers (Titan):
   ValidationException: Agreement not supported for this model
   (expected for Amazon first-party models)

5) IAM user isolation:
   Created user lambo-bedrock-test with inline policy allowing
   bedrock:InvokeModel, InvokeModelWithResponseStream,
   GetFoundationModelAvailability, ListFoundationModels.
   PermissionsBoundary: none.
   Access keys for the user returned InvalidClientTokenId /
   UnrecognizedClientException on sts:GetCallerIdentity and Bedrock
   (keys deleted after test).

6) IAM role isolation:
   Created role LamboBedrockTestRole with the same Bedrock allow policy.
   sts:AssumeRole as root failed with:
   "Roles may not be assumed by root accounts."

7) Payment method was already configured from account start.
   EC2 free-tier activity already performed in us-east-1.
   put-use-case-for-model-access previously failed: account not authorized.
   Waited ~24h after first non-Bedrock activity and retried — still NOT_AUTHORIZED.

Request:
Please perform backend account-level Bedrock authorization remediation so
authorizationStatus becomes AUTHORIZED for amazon.titan-embed-text-v2:0
(and foundation model playground / InvokeModel works) on account <AWS_ACCOUNT_ID>.

Use case: hackathon agentic memory (Lambo) needing Titan Text Embeddings V2 (1024-dim).
```

---

## Lambo engineering stance while waiting

| Area | Stance |
|------|--------|
| P0 T0.1–T0.3 | Done; Rust GO on Cockroach VECTOR |
| P0 T0.4 | **Blocked on AWS account** — spike ready (`spikes/bedrock-spike`) |
| P1 | Contracts + MemoryStore + FixtureEmbedder; fixtures T1.4 next |
| Production path | Keyword-only / capability-gated hybrid until Titan works |
| Env | `.env` may hold `AWS_BEARER_TOKEN_BEDROCK` (short-lived); prefer `aws login` after unlock; never commit `.env` |

---

## Timeline note

| Event | When (approx) |
|-------|----------------|
| EC2 free-tier launch + Lambo build on instance | 2026-08-10 ~12:00–12:20 UTC |
| Support self-service steps executed | 2026-08-10 ~12:50 UTC |
| **Earliest retest** | **~2026-08-11 12:00 UTC** (24h after EC2 activity) |
| Record retest result here | _fill in_ |

### Retest log

```text
Date/time:
authorizationStatus:
InvokeModel:
Spike:
Next action:
```
