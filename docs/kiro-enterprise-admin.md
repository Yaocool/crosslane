# Kiro Enterprise administration

[English](kiro-enterprise-admin.md) | [简体中文](kiro-enterprise-admin.zh-CN.md) | [Project overview](../README.md)

Base URL: `http://127.0.0.1:7651/admin/v1/kiro/enterprise`.
Uses the independent [Admin REST listener](admin-rest.md), its Bearer authentication and roles.
Read-only tokens permit GET/HEAD; activation, cancellation and plan changes require full access.

These resources represent AWS/Kiro enterprise subscriptions. Local Crosslane accounts still use
`/admin/v1/accounts`. Activation does not perform browser SSO or import an account into a local pool.

[CLI](#manage-subscriptions-from-the-cli) · [AWS credentials](#daemon-credentials) · [Endpoints](#endpoints) · [User queries](#query-users-and-subscriptions) · [HTTP mutations](#http-subscription-changes) · [Permissions](#aws-permissions-and-diagnostics)

## Manage subscriptions from the CLI

First configure AWS credentials with `crosslane admin kiro configure`. Subscription
commands use the owner-only daemon Unix socket and those credentials; **no Admin
token or REST/status listener is required**. Upgrade both the CLI and daemon. Docker
callers need access to the management socket or must run inside the container.

| Command (prefix `crosslane admin kiro subscription`) | Purpose |
| --- | --- |
| `plans` | Static plan list; does not contact AWS or validate permissions |
| `profiles` | Verify AWS identity and list profiles in the selected region |
| `scope` | Discover and validate the profile, Identity Center instance, application and identity store |
| `users` | All identity-store users with direct subscriptions; application GROUP claims listed separately |
| `list` | All application USER/GROUP claims, including pending plans |
| `show <USER>` | Direct and currently inherited group subscriptions |
| `add <USER> --plan <PLAN>` | Activate a direct subscription; reject an existing one |
| `upgrade <USER> --plan <PLAN>` | Upgrade a unique direct subscription to a higher plan |
| `downgrade <USER> --plan <PLAN>` | Downgrade a unique direct subscription to a lower plan |
| `cancel <USER> [--mode end-of-period\|immediate]` | Cancel a direct subscription; default at period end |

```bash
crosslane admin kiro subscription users
crosslane --json admin kiro subscription list
crosslane admin kiro subscription show user@example.com

# Read-only previews; removing --dry-run submits the actual change.
crosslane admin kiro subscription add user@example.com --plan pro --dry-run
crosslane admin kiro subscription upgrade user@example.com --plan power --dry-run
crosslane admin kiro subscription downgrade user@example.com --plan pro --dry-run
crosslane admin kiro subscription cancel user@example.com --dry-run
```

`USER` accepts an exact Identity Center username, UserId, or user ARN without URL
encoding. Plans are `pro`, `pro-plus`, `pro-max`, and `power`. Scope selectors
`--region`, `--profile-arn`, `--application-arn`, `--identity-center-region`, and
`--legacy` follow the action; `profiles` accepts only region/legacy. Omitted values
use saved defaults and discovery. Legacy mode supports only `us-east-1`, requires
explicit `--mode immediate` for cancellation, and is never selected automatically.
Use `crosslane help admin kiro subscription --all` for aliases and full options.

Mutations write by default. A dry run checks discovery and operation conditions,
not AWS write permissions. CLI and REST share user locks, eight mutation slots,
and completion audits (`source=unix_socket` for CLI). After a timeout or uncertain
write, query with `show` before retrying; see [subscription semantics](#semantics-and-diagnostics).
The separate top-level `crosslane subscriptions --provider kiro` queries available
plans with local account credentials and does not manage enterprise assignments.

## Daemon credentials

### CLI configuration and hot updates (recommended)

Configure through the owner-only daemon Unix socket; no existing Admin token is required:

```bash
crosslane admin kiro configure
crosslane admin kiro show
crosslane --json admin kiro show
```

`configure` hides AK, SK and optional STS SessionToken input, then asks for the region
(default `us-east-1`) and optional Profile ARN, Application ARN and Identity Center region.
It replaces the complete credential/scope set; omitted scope fields revert to defaults.
Temporary `ASIA` keys require a SessionToken. For automation, use
`crosslane admin kiro configure --stdin < /secure/kiro-admin-input.json` with a `0600` input file,
or pipe JSON directly from your credential manager:

```json
{
  "access_key_id": "<AccessKeyId>",
  "secret_access_key": "<SecretAccessKey>",
  "region": "us-east-1"
}
```

Optional fields: `session_token`, `profile_arn`, `application_arn`, `identity_center_region`.
Secrets are not accepted in argv, printed, or included in input error diagnostics. Results contain
only configuration status, source, region, scope and token presence. Configured status means local
syntax checks passed; use `GET /profiles` to verify AWS identity/permissions. `GET /plans` is static.

Profile ARNs validate the CodeWhisperer service, region, account and profile resource structure;
Application ARNs validate the IAM Identity Center application structure. The resource types cannot
be interchanged. See the [AWS Profile ARN definition](https://docs.aws.amazon.com/service-authorization/latest/reference/list_codewhisperer.html)
and [AWS Application ARN definition](https://docs.aws.amazon.com/singlesignon/latest/APIReference/API_DescribeApplication.html).

Write responses describe the submitted snapshot; use `show` for current state after concurrent changes.
A `persistence_warning` means the change is already live but directory sync failed, so crash durability
is unconfirmed. Terminal output displays it and JSON retains the field. Fix storage and retry the
requested operation; do not assume the change was rejected. Ordinary `show` is not a durability check.

Subsequent Kiro management requests use saved credentials immediately, without `config reload`,
a restart or container recreation. Accepted AWS operations retain their original credential snapshot.
Business services, account pools and REST tokens are unaffected.

Credentials are AES-256-GCM encrypted in `.secrets/kiro-admin.enc` under the **daemon data directory**;
a random local encryption key is stored alongside it as `kiro-admin.key`. The directory uses `0700`
and files `0600`; ownership is checked and symbolic/hard links or permissive modes are rejected.
macOS extended ACLs are also checked: additional ALLOW entries are rejected even when mode bits look
private, while restrictive DENY entries remain supported. Inspect and remove inherited extra grants
before configuring credentials; the CLI does not overwrite existing ACLs or parent-directory policies.
Corruption fails closed without reverting to environment credentials. These credentials are outside
`config.toml`, ordinary configuration/account exports, REST responses and logs; `config reset` does
not modify them. Native defaults use `~/.local/share/crosslane/.secrets` (respecting `XDG_DATA_HOME`)
or `$CROSSLANE_HOME/.secrets`. Docker stores them in its persistent volume at
`/var/lib/crosslane/.secrets`; the calling CLI does not keep a local copy.

Protect backups of both the encrypted file and local key: possession of both permits decryption.
Restore a lost local key or fully replace the credentials with `configure`.

```bash
crosslane admin kiro clear    # Remove credentials and persist a disabled override
crosslane admin kiro use-env  # Explicitly restore the daemon's inherited environment
```

The file takes precedence as a complete set; keys, token and scope are never mixed with environment
values. `clear` prevents old environment credentials from returning, including after restart.
`use-env` persists explicit environment selection; invalid/incomplete environment credentials reject the switch.

### Legacy environment configuration

The following remains compatible when no private credentials are configured or `use-env` was explicitly selected. Leave these fields empty
when using CLI-managed credentials.

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

The repository [Compose configuration](../docker-compose.yml) forwards these variables.
Set the host `.env` using [.env.example](../.env.example); custom Compose files need
the corresponding environment mappings.

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
| GET | `/users` | Usernames, display names, emails, and direct claims for all directory users; group claims separately |
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
override the active private credential or legacy environment defaults. Discovery selects only a unique candidate; ambiguity returns 409.
Profiles must match the STS caller's account and requested region. Applications must be associated
with the selected Profile and its Identity Center instance. Commercial AWS partitions only; China
and GovCloud are unsupported. Explicit `legacy=true` supports only us-east-1 and immediate
cancellation; no automatic legacy fallback occurs.

## Query users and subscriptions

Use `/users` when you need `user_name`, `display_name`, and `emails`. It calls
Identity Store `ListUsers` and joins direct claims by UserId. `/subscriptions`
returns claim IDs, plans, and status only; it does not join directory details.

`/users` lists **all directory users**, including those without direct subscriptions.
Application group claims appear separately in `group_subscriptions`, without
expanding members. An empty `direct_subscriptions` array does not exclude inherited
access. Use `/users/{user}/subscription` (CLI `show`) for one user's direct and
currently inherited claims. Missing directory names remain null; missing emails
produce an empty array. Queries cover the selected account/region/application,
not all AWS accounts or regions.

```bash
BASE=http://127.0.0.1:7651/admin/v1/kiro/enterprise
# Set CROSSLANE_ADMIN_TOKEN to your existing Admin token.
curl -sS -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/profiles"
curl -sS -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/scope"
curl -sS -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/users?region=us-east-1"
curl -sS -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" "$BASE/subscriptions?region=us-east-1"
```

The HTTP user query needs no body. Replace the listener address and token:

```http
GET /admin/v1/kiro/enterprise/users?region=us-east-1 HTTP/1.1
Host: 127.0.0.1:7651
Authorization: Bearer <CROSSLANE_ADMIN_TOKEN>
Accept: application/json
```

Example response with fictional data:

```http
HTTP/1.1 200 OK
Content-Type: application/json
Cache-Control: no-store

{
  "data": {
    "scope": {
      "account_id": "123456789012",
      "kiro_region": "us-east-1",
      "identity_center_region": "us-east-1",
      "identity_store_id": "d-1111222233",
      "instance_arn": "arn:aws:sso:::instance/ssoins-1111222233334444",
      "application_arn": "arn:aws:sso::123456789012:application/ssoins-1111222233334444/apl-1111222233334444",
      "profile_arn": "arn:aws:codewhisperer:us-east-1:123456789012:profile/example",
      "legacy": false
    },
    "users": [
      {
        "user_id": "11111111-1111-4111-8111-111111111111",
        "user_name": "user@example.com",
        "display_name": "Example User",
        "emails": [
          "user@example.com"
        ],
        "direct_subscriptions": [
          {
            "principal_type": "USER",
            "principal_id": "11111111-1111-4111-8111-111111111111",
            "subscription_type": "KIRO_ENTERPRISE_PRO",
            "plan": "pro",
            "pending_subscription_type": null,
            "pending_plan": null,
            "status": "ACTIVE"
          }
        ]
      }
    ],
    "group_subscriptions": []
  }
}
```

`plan` is the current tier; `pending_plan` is the pending tier or null. The underlying
`subscription_type` fields preserve AWS values, including unknown tiers. For formatted
file export and HTTP error diagnosis, use [Save and inspect JSON responses](admin-rest.md#save-and-inspect-json-responses);
replace that example's URL with the full `/users` URL above.

## HTTP subscription changes

These calls require a full-access Admin token. Writes submit actual changes unless
`dry_run` is explicitly enabled.

```bash
BASE=http://127.0.0.1:7651/admin/v1/kiro/enterprise
USER_ID=REPLACE_WITH_IDENTITY_CENTER_USER_ID

# Read-only preflight: no subscription write is submitted.
curl -fsS -X POST -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-plus","dry_run":true}' \
  "$BASE/users/$USER_ID/subscription"

# Activate; Kiro may assign the application internally, without a duplicate client call.
curl -fsS -X POST -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-plus"}' \
  "$BASE/users/$USER_ID/subscription"

# Upgrade from a lower current plan.
curl -fsS -X POST -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro-max"}' \
  "$BASE/users/$USER_ID/subscription/upgrade"

# Downgrade from a higher current plan.
curl -fsS -X POST -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  -H 'Content-Type: application/json' -d '{"plan":"pro"}' \
  "$BASE/users/$USER_ID/subscription/downgrade"

# End-of-month access removal (also the default).
curl -fsS -X DELETE -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  "$BASE/users/$USER_ID/subscription?mode=end_of_period"

# Immediate direct-subscription access removal; billing still ends at month end.
curl -fsS -X DELETE -H "Authorization: Bearer $CROSSLANE_ADMIN_TOKEN" \
  "$BASE/users/$USER_ID/subscription?mode=immediate"
```

All POST mutations support JSON `dry_run=true`; DELETE accepts query `dry_run=true`. Previews
return 200/`preview`, accepted writes return 202/`api_accepted`. Both need full Admin privileges.
Previews validate discovery and the requested operation without calling the AWS write APIs; they
cannot verify assignment or downstream claim write permissions.
`application_assignment_requested` is always false. Additional fields such as `assign_application`
are rejected. The flag means that Crosslane did not send a separate
CreateApplicationAssignment/DeleteApplicationAssignment request. Kiro can perform application
assignments internally while processing its Assignment RPCs; false does not mean the user remains
unassigned. Crosslane does not duplicate these AWS-managed calls, create/delete Identity Center
users, or change group memberships.

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

## AWS permissions and diagnostics

See [Kiro IAM](https://kiro.dev/docs/enterprise/iam/) for permissions. Subscription operations use
`q:CreateAssignment`, `q:UpdateAssignment`, `q:DeleteAssignment`. The caller also needs
`user-subscriptions:CreateClaim`, `user-subscriptions:UpdateClaim` and
`user-subscriptions:DeleteClaim` for downstream subscription writes performed by Kiro on the
caller's behalf. Assignment permissions alone do not authorize these downstream calls. Queries use
`user-subscriptions:ListApplicationClaims`. Discovery uses `codewhisperer:ListProfiles`,
`sso:DescribeInstance`, `sso:DescribeApplication`, `identitystore:ListUsers`,
`identitystore:GetUserId`, `identitystore:DescribeUser`, `identitystore:ListGroupMembershipsForMember`
and STS caller identity. AWS resource permissions, SCPs and cross-account constraints still apply.
The caller also needs `sso-directory:DescribeUser`: User Subscriptions can call this on the
caller's behalf during CreateClaim. `identitystore:DescribeUser` is a different IAM action and does
not authorize it. Kiro can then call `sso:CreateApplicationAssignment` on the caller's behalf, so
activation also needs that permission even though Crosslane sends no separate SSO write.
In an existing profile/application, the observed activation chain is:

```text
q:CreateAssignment
  -> user-subscriptions:CreateClaim
       -> sso-directory:DescribeUser
  -> sso:CreateApplicationAssignment
```

The Kiro console additionally uses actions such as `sso-directory:SearchUsers`,
`sso-directory:DescribeUsers`, `sso:ListApplicationAssignments` and
`user-subscriptions:ListUserSubscriptions` for its UI. Seeing those in console history does not
make them dependencies of Crosslane's activation request. Profile create/update/delete permissions
are likewise not needed merely to activate a subscription in an existing profile.

For an assignment `AccessDenied`, inspect both the `q.amazonaws.com` assignment event and the
downstream `user-subscriptions.amazonaws.com` claim event in CloudTrail. The latter can identify
the missing permission even when CreateAssignment returns only a generic denial. Include read-only
events too: a failed `sso-directory.amazonaws.com` DescribeUser can be hidden by a generic
CreateClaim denial. Filter by caller and a narrow time range rather than only `ReadOnly=false`,
then correlate the target user, application and `invokedBy` fields. A claim event
whose `sourceIPAddress` is `q.amazonaws.com` does not itself prove that an `aws:SourceIp`
condition failed: [AWS documents](https://docs.aws.amazon.com/IAM/latest/UserGuide/access_forward_access_sessions.html)
that forward access sessions following a public endpoint request retain its original source IP.
Requests through VPC endpoints require separate handling of the documented network conditions.

### Backend protocol

Subscription/claim operations use Kiro Console backend protocols rather than a versioned public
AWS SDK service contract; see the [Kiro repository request example](https://github.com/kirodotdev/Kiro/issues/10166).
Requests use SigV4 and fixed hosts derived from validated regions. Arbitrary endpoints are not
accepted.
