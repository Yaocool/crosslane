# Kiro Enterprise administration

Base URL: `http://127.0.0.1:7651/admin/v1/kiro/enterprise`.
Uses the independent [Admin REST listener](admin-rest.md), its Bearer authentication and roles.
Read-only tokens permit GET/HEAD; activation, cancellation and plan changes require full access.

These resources represent AWS/Kiro enterprise subscriptions. Local Crosslane accounts still use
`/admin/v1/accounts`. Activation does not perform browser SSO or import an account into a local pool.

## Daemon credentials

Inject credentials into the **crosslaned process**, then start/restart it. For containers, inject
the container environment; exporting variables in a CLI shell does not modify a running daemon.

```bash
export CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID='REPLACE_WITH_ADMIN_ACCESS_KEY_ID'
export CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY='REPLACE_WITH_ADMIN_SECRET_ACCESS_KEY'
export CROSSLANE_KIRO_ADMIN_REGION='us-east-1'
# Required for STS credentials; unset or empty is allowed for long-lived keys.
export CROSSLANE_KIRO_ADMIN_SESSION_TOKEN='REPLACE_WITH_STS_SESSION_TOKEN'
crosslaned
```

Credentials are never written to Crosslane TOML/account storage or returned by REST. The default
AWS credential chain, AWS profiles, SSO caches and instance/container metadata are not consulted.
REST callers supply only the Admin token, never AWS credentials. Missing/incomplete credentials
produce `503 kiro_admin_not_configured`; `/plans` works without AWS configuration. Changes to the
process environment require a daemon restart, not `config reload`.

| Environment variable | Purpose |
| --- | --- |
| `CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID` | Required administrator access key in the AWS account owning the Kiro Profile |
| `CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY` | Required secret key |
| `CROSSLANE_KIRO_ADMIN_SESSION_TOKEN` | Required for STS credentials; unset or empty for long-lived keys |
| `CROSSLANE_KIRO_ADMIN_REGION` | Kiro Profile region; defaults to `us-east-1` |
| `CROSSLANE_KIRO_ADMIN_PROFILE_ARN` | Optional explicit Profile when discovery is ambiguous |
| `CROSSLANE_KIRO_ADMIN_APPLICATION_ARN` | Optional selection among applications associated with that Profile |
| `CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION` | Optional fallback when the Profile lacks `ssoRegion`; cannot override a different recorded region |

The repository's default `docker-compose.yml` forwards all variables above. Configure
the host `.env` using `.env.example`; custom Compose files need equivalent mappings, for example:

```yaml
services:
  crosslaned:
    environment:
      CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID: ${CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID:-}
      CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY: ${CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY:-}
      CROSSLANE_KIRO_ADMIN_REGION: ${CROSSLANE_KIRO_ADMIN_REGION:-us-east-1}
      CROSSLANE_KIRO_ADMIN_SESSION_TOKEN: ${CROSSLANE_KIRO_ADMIN_SESSION_TOKEN:-}
      CROSSLANE_KIRO_ADMIN_PROFILE_ARN: ${CROSSLANE_KIRO_ADMIN_PROFILE_ARN:-}
      CROSSLANE_KIRO_ADMIN_APPLICATION_ARN: ${CROSSLANE_KIRO_ADMIN_APPLICATION_ARN:-}
      CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION: ${CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION:-}
```

Empty optional environment variables are treated as unset; temporary `ASIA` credentials still
require a nonempty session token. Explicit empty Profile/Application query selectors remain invalid.

After changing the Compose `.env` or environment mappings, recreate the service with
`docker compose up -d --force-recreate crosslaned`.
[`docker compose restart` does not reload environment changes](https://docs.docker.com/reference/cli/docker/compose/restart/).

## Endpoints

Paths below are relative to `/admin/v1/kiro/enterprise`.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/plans` | Supported plan names and their AWS subscription types |
| GET | `/profiles` | Verify the AWS caller and discover profiles in that account/region |
| GET | `/scope` | Resolve/validate Profile, Identity Center instance, application and identity store |
| GET | `/users` | All directory users and their direct USER claims; application GROUP claims separately |
| GET | `/subscriptions` | All USER/GROUP claims for the application, including pending plans |
| GET | `/users/{user}/subscription` | Direct claims and claims inherited through current group memberships |
| POST | `/users/{user}/subscription` | Activate a direct USER subscription; existing direct claims return 409 |
| POST | `/users/{user}/subscription/upgrade` | Strict upgrade |
| POST | `/users/{user}/subscription/downgrade` | Strict downgrade |
| DELETE | `/users/{user}/subscription` | Cancel the direct subscription, preserving the directory user |

`{user}` accepts an exact UserId (including identity-store-prefixed UUIDs), commercial-partition
Identity Store user ARN, or userName, not a display name. URL-encode ARNs in path parameters,
especially their slash. Valid user/group/store ARN and UUID references are canonicalized to bare
IDs before scope checks and binding/inheritance matching. Wrong resource types, services and
partitions are not reinterpreted as usernames. Subscription writes always send bare UserIds.
When the identity store owner is known, returned ARN ownership must match it too; matching only
the final resource ID does not authorize a different account's ARN.
User listing does not expand every user's group memberships; the individual subscription
endpoint does. Usernames are limited to 128 Unicode characters, with whitespace/control characters
rejected before any AWS request. Returned users/memberships must match the discovered identity
store; returned membership user IDs must match the selected user.
Lists are bounded at 100 pages/10,000 items/16 MiB in aggregate and 4 MiB per upstream response.
Malformed pagination or exhausted bounds fail instead of returning incomplete results. An operation
has a 120-second AWS budget, with at most 40 seconds per HTTP submission.

Optional `region`, `profile_arn`, `application_arn`, and `identity_center_region` query parameters
override environment defaults. Discovery selects only a unique candidate; ambiguity returns 409.
Profiles must match the STS caller's account and requested region. Applications must be associated
with the selected Profile and its Identity Center instance. Commercial AWS partitions only; China
and GovCloud are unsupported. Explicit `legacy=true` supports only us-east-1 and immediate
cancellation; no automatic legacy fallback occurs.

## Callers

Only the Crosslane Admin token is needed by callers:

```bash
BASE=http://127.0.0.1:7651/admin/v1/kiro/enterprise
ADMIN_TOKEN='REPLACE_WITH_CROSSLANE_ADMIN_TOKEN'
USER_ID='REPLACE_WITH_IDENTITY_CENTER_USER_ID'

curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/profiles"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/scope"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/users"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/subscriptions"
curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE/users/$USER_ID/subscription"

# Read-only preflight: no subscription write is submitted.
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-plus","dry_run":true}' \
  "$BASE/users/$USER_ID/subscription"

# Activate without a separate application assignment.
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-plus"}' \
  "$BASE/users/$USER_ID/subscription"

# Upgrade from a lower current plan.
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-max"}' \
  "$BASE/users/$USER_ID/subscription/upgrade"

# Downgrade from a higher current plan.
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro"}' \
  "$BASE/users/$USER_ID/subscription/downgrade"

# End-of-month access removal (also the default).
curl -fsS -X DELETE -H "Authorization: Bearer $ADMIN_TOKEN" \
  "$BASE/users/$USER_ID/subscription?mode=end_of_period"

# Immediate direct-subscription access removal; billing still ends at month end.
curl -fsS -X DELETE -H "Authorization: Bearer $ADMIN_TOKEN" \
  "$BASE/users/$USER_ID/subscription?mode=immediate"
```

All POST mutations support JSON `dry_run=true`; DELETE accepts query `dry_run=true`. Previews
return 200/`preview`, accepted writes return 202/`api_accepted`. Both need full Admin privileges.
`application_assignment_requested` is always false. Additional fields such as `assign_application`
are rejected. Crosslane never calls CreateApplicationAssignment/DeleteApplicationAssignment,
creates/deletes an Identity Center user, or changes a group assignment.

At most eight subscription mutations/previews can run concurrently; saturation returns `503 busy`.
Overlapping operations for the same AWS account, region and resolved user return 409. Username/UUID
aliases share the guard, while different users can run concurrently. Disconnecting the HTTP caller
does not cancel an already started operation: it retains capacity and its user guard until completion or
the existing AWS deadline. `admin.kiro.subscription.completed` records the server request ID,
Admin token ID/role, operation, status, error code and available AWS request ID, without credentials,
token plaintext or request bodies. Exclusion applies within this Admin REST instance; it
cannot make updates atomic against other Console/API writers. After 202, verify current/pending
plans before submitting a subsequent change.
Catchable execution panics still produce completion audits with an unknown outcome, without
returning the panic payload. Audit `write_outcome` uses `accepted`, `not_submitted`, `rejected`
or `unknown` consistently.

## Semantics and diagnostics

[Official Kiro rules](https://kiro.dev/docs/enterprise/subscription-management/): upgrades normally
take effect immediately and downgrades next month. Cancellation can remove access immediately or
at month end; both end the subscription at month end. Group-inherited entitlements may remain
after a direct USER subscription is cancelled.

Upgrade/downgrade requires exactly one direct USER claim with a recognized current tier and the
correct direction. Unknown tiers, ambiguous claims or mismatched directions return 409 before a
write. Current (`subscription_type`, `plan`) and pending (`pending_subscription_type`, `pending_plan`)
tiers remain separate.

Writes are submitted once, with no application-level retry, redirects or endpoint/region fallback.
202 confirms API acceptance, not effective access, billing or plan completion; query state or check
the AWS Console afterward. AWS operation errors include operation, upstream status, filtered upstream code,
request ID and `write_outcome`, never raw upstream error bodies, credentials or signatures:

AWS error envelopes in HTTP 2xx responses are failures too; unrecognized outcomes remain `unknown`.
If response streaming fails after headers, the received HTTP status and request ID are retained.

- `not_submitted`: preflight failed; no subscription write was sent.
- `rejected`: the upstream explicitly rejected the write.
- `unknown`: a write timed out, failed in transit, hit a server failure/unexpected redirect, or returned an invalid
  response. It may already have taken effect; query before retrying.

See [Kiro IAM](https://kiro.dev/docs/enterprise/iam/) for permissions. Subscription operations use
`q:CreateAssignment`, `q:UpdateAssignment`, `q:DeleteAssignment` and
`user-subscriptions:ListApplicationClaims`. Discovery uses `codewhisperer:ListProfiles`,
`sso:DescribeInstance`, `sso:DescribeApplication`, `identitystore:ListUsers`,
`identitystore:GetUserId`, `identitystore:DescribeUser`, `identitystore:ListGroupMembershipsForMember`
and STS caller identity. AWS resource permissions, SCPs and cross-account constraints still apply.
Crosslane does not need CreateApplicationAssignment/DeleteApplicationAssignment permissions.

Subscription/claim operations use Kiro Console backend protocols rather than a versioned public
AWS SDK service contract; see the [Kiro repository request example](https://github.com/kirodotdev/Kiro/issues/10166).
Requests use SigV4 and fixed hosts derived from validated regions. Arbitrary endpoints are not
accepted. Local mock protocol tests cover this implementation; no real AWS activation,
cancellation or billing change was used to validate it.
