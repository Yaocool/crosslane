//! Kiro Enterprise Console administration, separate from OAuth account pools.
//! Credentials and discovery state live for one operation only. Identity Center
//! is never written directly here. Kiro's Assignment RPCs can perform their own
//! application-assignment writes inside AWS; we do not duplicate those calls.
mod coordinator;
mod identity;
mod transport;
mod types;

pub use coordinator::MutationCoordinator;
pub use types::*;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};

use transport::{AwsTransport, Service};

const MAX_PAGES: usize = 100;
const MAX_ITEMS: usize = 10_000;
const MAX_COLLECTION_BYTES: usize = 16 * 1024 * 1024;

pub struct EnterpriseClient {
    transport: AwsTransport,
    mutations: Arc<MutationCoordinator>,
}

impl EnterpriseClient {
    pub fn new(
        credentials: AwsCredentials,
        region: String,
        legacy: bool,
    ) -> Result<Self, EnterpriseError> {
        Ok(Self {
            transport: AwsTransport::new(credentials, region, legacy)?,
            mutations: Arc::new(MutationCoordinator::default()),
        })
    }

    pub fn with_mutation_coordinator(mut self, mutations: Arc<MutationCoordinator>) -> Self {
        self.mutations = mutations;
        self
    }

    pub async fn profiles(&self) -> Result<Profiles, EnterpriseError> {
        let account_id = self.transport.caller_account().await?;
        let raw = self
            .pages(
                Service::Profiles,
                &self.transport.region,
                "ListProfiles",
                json!({}),
                "profiles",
                "nextToken",
            )
            .await?;
        let mut profiles = Vec::new();
        for value in raw {
            let arn = required(&value, "arn")?;
            let parts: Vec<_> = arn.splitn(6, ':').collect();
            if parts.len() != 6
                || parts[0] != "arn"
                || parts[1] != "aws"
                || parts[3] != self.transport.region
                || parts[4] != account_id
                || !parts[5].starts_with("profile/")
            {
                return Err(EnterpriseError::protocol(
                    "profile account or region does not match the authenticated AWS identity",
                ));
            }
            let sso = value.pointer("/identityDetails/ssoIdentityDetails");
            let mut applications = BTreeSet::new();
            if let Some(properties) = value.get("applicationProperties") {
                for property in properties
                    .as_array()
                    .ok_or_else(|| EnterpriseError::protocol("invalid profile application list"))?
                {
                    if let Some(arn) = property
                        .get("applicationArn")
                        .and_then(Value::as_str)
                        .filter(|value| is_application_arn(value))
                    {
                        applications.insert(arn.to_owned());
                    }
                }
            }
            if let Some(arn) = sso
                .and_then(|value| value.get("oidcClientId"))
                .and_then(Value::as_str)
                .filter(|value| is_application_arn(value))
            {
                applications.insert(arn.to_owned());
            }
            profiles.push(EnterpriseProfile {
                arn: arn.to_owned(),
                instance_arn: sso
                    .and_then(|value| value.get("instanceArn"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                identity_center_region: sso
                    .and_then(|value| value.get("ssoRegion"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                application_arns: applications.into_iter().collect(),
            });
        }
        Ok(Profiles {
            account_id,
            kiro_region: self.transport.region.clone(),
            profiles,
        })
    }

    pub async fn scope(
        &self,
        selector: &ScopeSelector,
    ) -> Result<EnterpriseScope, EnterpriseError> {
        for value in [&selector.profile_arn, &selector.application_arn] {
            if value.as_ref().is_some_and(|value| {
                value.is_empty() || value.len() > 2048 || value != value.trim()
            }) {
                return Err(EnterpriseError::invalid(
                    "profile and application selectors must be nonempty ARNs",
                ));
            }
        }
        if let Some(region) = &selector.identity_center_region {
            types::validate_region(region)?;
        }
        let profiles = self.profiles().await?;
        let profile = match selector.profile_arn.as_deref() {
            Some(arn) => profiles
                .profiles
                .iter()
                .find(|profile| profile.arn == arn)
                .ok_or_else(|| EnterpriseError::not_found("Kiro profile not found"))?,
            None if profiles.profiles.len() == 1 => &profiles.profiles[0],
            None if profiles.profiles.is_empty() => {
                return Err(EnterpriseError::not_found("no Kiro profile found"))
            }
            None => {
                return Err(EnterpriseError::conflict(
                    "multiple Kiro profiles; specify profile_arn",
                ))
            }
        };
        let instance_arn = profile
            .instance_arn
            .clone()
            .ok_or_else(|| EnterpriseError::invalid("profile does not use IAM Identity Center"))?;
        let identity_center_region = match (
            &profile.identity_center_region,
            &selector.identity_center_region,
        ) {
            (Some(actual), Some(requested)) if actual != requested => {
                return Err(EnterpriseError::invalid(
                    "identity_center_region does not match the profile",
                ))
            }
            (Some(region), _) | (None, Some(region)) => region.clone(),
            (None, None) => {
                return Err(EnterpriseError::invalid(
                    "profile has no SSO region; specify identity_center_region",
                ))
            }
        };
        types::validate_region(&identity_center_region)?;
        let application_arn = match selector.application_arn.as_ref() {
            Some(arn) if profile.application_arns.contains(arn) => arn.clone(),
            Some(_) => {
                return Err(EnterpriseError::invalid(
                    "application_arn is not associated with the selected profile",
                ))
            }
            None if profile.application_arns.len() == 1 => profile.application_arns[0].clone(),
            None if profile.application_arns.is_empty() => {
                return Err(EnterpriseError::invalid(
                    "profile has no associated Identity Center application",
                ))
            }
            None => {
                return Err(EnterpriseError::conflict(
                    "multiple associated applications; specify application_arn",
                ))
            }
        };
        let instance = self
            .transport
            .call(
                Service::Sso,
                &identity_center_region,
                "DescribeInstance",
                json!({"InstanceArn":instance_arn}),
            )
            .await?
            .data;
        if instance
            .get("InstanceArn")
            .and_then(Value::as_str)
            .is_some_and(|arn| arn != instance_arn)
        {
            return Err(EnterpriseError::protocol(
                "Identity Center instance response does not match the profile",
            ));
        }
        let identity_store_id = identity::store_id(required(&instance, "IdentityStoreId")?)
            .ok_or_else(|| {
                EnterpriseError::protocol("Identity Center returned an invalid identity store ID")
            })?;
        let identity_store_account = match instance
            .get("OwnerAccountId")
            .filter(|value| !value.is_null())
        {
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|account| {
                        account.len() == 12 && account.bytes().all(|byte| byte.is_ascii_digit())
                    })
                    .ok_or_else(|| {
                        EnterpriseError::protocol(
                            "Identity Center returned an invalid owner account",
                        )
                    })?
                    .to_owned(),
            ),
            None => {
                identity::store_account(required(&instance, "IdentityStoreId")?).map(str::to_owned)
            }
        };
        if identity::store_account(required(&instance, "IdentityStoreId")?).is_some_and(|owner| {
            identity_store_account
                .as_deref()
                .is_some_and(|expected| owner != expected)
        }) {
            return Err(EnterpriseError::protocol(
                "identity store ARN does not match the Identity Center owner",
            ));
        }
        let application = self
            .transport
            .call(
                Service::Sso,
                &identity_center_region,
                "DescribeApplication",
                json!({"ApplicationArn":application_arn}),
            )
            .await?
            .data;
        if application.get("InstanceArn").and_then(Value::as_str) != Some(instance_arn.as_str())
            || application
                .get("ApplicationArn")
                .and_then(Value::as_str)
                .is_some_and(|arn| arn != application_arn)
        {
            return Err(EnterpriseError::protocol(
                "application does not belong to the profile's Identity Center instance",
            ));
        }
        Ok(EnterpriseScope {
            account_id: profiles.account_id,
            kiro_region: self.transport.region.clone(),
            identity_center_region,
            identity_store_id,
            instance_arn,
            application_arn,
            profile_arn: profile.arn.clone(),
            legacy: self.transport.legacy,
            identity_store_account,
        })
    }

    pub async fn subscriptions(
        &self,
        selector: &ScopeSelector,
    ) -> Result<(EnterpriseScope, Vec<SubscriptionClaim>), EnterpriseError> {
        let scope = self.scope(selector).await?;
        let claims = self.claims(&scope).await?;
        Ok((scope, claims))
    }

    pub async fn users(&self, selector: &ScopeSelector) -> Result<UserList, EnterpriseError> {
        let scope = self.scope(selector).await?;
        let claims = self.claims(&scope).await?;
        let raw = self
            .pages(
                Service::IdentityStore,
                &scope.identity_center_region,
                "ListUsers",
                json!({"IdentityStoreId":scope.identity_store_id,"MaxResults":100}),
                "Users",
                "NextToken",
            )
            .await?;
        let mut direct: HashMap<&str, Vec<SubscriptionClaim>> = HashMap::new();
        for claim in claims.iter().filter(|claim| claim.principal_type == "USER") {
            direct
                .entry(&claim.principal_id)
                .or_default()
                .push(claim.clone());
        }
        let mut users = Vec::new();
        let mut seen = HashSet::new();
        for value in raw {
            verify_identity_store(&value, &scope)?;
            let mut user = EnterpriseUser::parse(&value, &[])?;
            if !seen.insert(user.user_id.clone()) {
                return Err(EnterpriseError::protocol(
                    "duplicate user ID in paginated directory listing",
                ));
            }
            user.direct_subscriptions = direct.remove(user.user_id.as_str()).unwrap_or_default();
            users.push(user);
        }
        Ok(UserList {
            scope,
            users,
            group_subscriptions: claims
                .into_iter()
                .filter(|claim| claim.principal_type == "GROUP")
                .collect(),
        })
    }

    pub async fn user_subscription(
        &self,
        selector: &ScopeSelector,
        user: &str,
    ) -> Result<UserSubscription, EnterpriseError> {
        validate_user_selector(user)?;
        let scope = self.scope(selector).await?;
        let claims = self.claims(&scope).await?;
        let user = self.resolve_user(&scope, user, &claims).await?;
        let memberships = self.pages(Service::IdentityStore, &scope.identity_center_region, "ListGroupMembershipsForMember",
            json!({"IdentityStoreId":scope.identity_store_id,"MemberId":{"UserId":user.user_id},"MaxResults":100}), "GroupMemberships", "NextToken").await?;
        let mut groups = HashSet::new();
        for membership in memberships {
            verify_identity_store(&membership, &scope)?;
            if let Some(member) = membership.get("MemberId").filter(|value| !value.is_null()) {
                if member
                    .get("UserId")
                    .and_then(Value::as_str)
                    .and_then(identity::user_id)
                    .is_none_or(|id| id != user.user_id)
                {
                    return Err(EnterpriseError::protocol(
                        "group membership does not belong to the requested user",
                    ));
                }
            }
            groups.insert(
                identity::group_reference(required(&membership, "GroupId")?).ok_or_else(|| {
                    EnterpriseError::protocol("Identity Center returned an invalid group reference")
                })?,
            );
        }
        let group_subscriptions = claims
            .into_iter()
            .filter(|claim| claim.principal_type == "GROUP" && groups.contains(&claim.principal_id))
            .collect();
        Ok(UserSubscription {
            scope,
            user,
            group_subscriptions,
        })
    }

    pub async fn mutate(
        &self,
        selector: &ScopeSelector,
        user: &str,
        mutation: Mutation,
        dry_run: bool,
    ) -> Result<MutationResult, EnterpriseError> {
        validate_user_selector(user)?;
        if self.transport.legacy && matches!(mutation, Mutation::Cancel(DeletionMode::EndOfPeriod))
        {
            return Err(EnterpriseError::invalid(
                "legacy control plane only supports immediate cancellation",
            ));
        }
        let scope = self.scope(selector).await?;
        let mut user = self.resolve_user(&scope, user, &[]).await?;
        let _mutation = self.mutations.try_begin(&scope, &user.user_id)?;
        // Read the plan only after acquiring the canonical-user guard.
        let claims = self.claims(&scope).await?;
        user.direct_subscriptions = claims
            .into_iter()
            .filter(|claim| claim.principal_type == "USER" && claim.principal_id == user.user_id)
            .collect();
        let previous_plan = user
            .direct_subscriptions
            .first()
            .and_then(|claim| claim.plan);
        let mut body = json!({"principalId":user.user_id,"principalType":"USER"});
        let expected_effect = match mutation {
            Mutation::Create(plan) => {
                if !user.direct_subscriptions.is_empty() {
                    return Err(EnterpriseError::conflict(
                        "user already has a direct subscription; query it before updating",
                    ));
                }
                body["subscriptionType"] = json!(plan.aws_type());
                "after_activation"
            }
            Mutation::Upgrade(plan) | Mutation::Downgrade(plan) => {
                if user.direct_subscriptions.len() != 1 {
                    return Err(EnterpriseError::conflict(
                        "a unique direct USER subscription is required to change plans",
                    ));
                }
                let current = previous_plan.ok_or_else(|| EnterpriseError::conflict("current plan is unknown; upgrade or downgrade direction cannot be verified"))?;
                let upgrade = matches!(mutation, Mutation::Upgrade(_));
                if (upgrade && plan.rank() <= current.rank())
                    || (!upgrade && plan.rank() >= current.rank())
                {
                    return Err(EnterpriseError::conflict(
                        "target plan does not match the requested upgrade or downgrade direction",
                    ));
                }
                body["subscriptionType"] = json!(plan.aws_type());
                if upgrade {
                    "immediate"
                } else {
                    "next_month"
                }
            }
            Mutation::Cancel(mode) => {
                if user.direct_subscriptions.is_empty() {
                    return Err(EnterpriseError::not_found(
                        "no direct USER subscription to cancel; group subscriptions are unaffected",
                    ));
                }
                if user.direct_subscriptions.len() != 1 {
                    return Err(EnterpriseError::conflict(
                        "a unique direct USER subscription is required to cancel",
                    ));
                }
                if !self.transport.legacy {
                    body["deletionMode"] = json!(mode.aws_type());
                }
                match mode {
                    DeletionMode::Immediate => "access_immediate_billing_end_of_month",
                    DeletionMode::EndOfPeriod => "end_of_month",
                }
            }
        };
        let request_id = if dry_run {
            None
        } else {
            self.transport
                .call(
                    Service::Assignments,
                    &scope.kiro_region,
                    mutation.operation(),
                    body,
                )
                .await?
                .request_id
        };
        Ok(MutationResult {
            status: if dry_run { "preview" } else { "api_accepted" },
            operation: mutation.operation(),
            scope,
            user_id: user.user_id,
            previous_plan,
            requested_plan: mutation.plan(),
            deletion_mode: if let Mutation::Cancel(mode) = mutation {
                Some(mode)
            } else {
                None
            },
            expected_effect,
            upstream_request_id: request_id,
            application_assignment_requested: false,
        })
    }

    async fn claims(
        &self,
        scope: &EnterpriseScope,
    ) -> Result<Vec<SubscriptionClaim>, EnterpriseError> {
        self.pages(Service::Subscriptions, &scope.identity_center_region, "ListApplicationClaims",
            json!({"applicationArn":scope.application_arn,"subscriptionRegion":scope.kiro_region,"maxResults":100}), "claims", "nextToken").await?
            .iter().map(SubscriptionClaim::parse).collect()
    }

    async fn resolve_user(
        &self,
        scope: &EnterpriseScope,
        selector: &str,
        claims: &[SubscriptionClaim],
    ) -> Result<EnterpriseUser, EnterpriseError> {
        validate_user_selector(selector)?;
        let id = if let Some(id) = identity::user_id(selector) {
            id
        } else {
            let result = self.transport.call(Service::IdentityStore, &scope.identity_center_region, "GetUserId",
                json!({"IdentityStoreId":scope.identity_store_id,"AlternateIdentifier":{"UniqueAttribute":{"AttributePath":"userName","AttributeValue":selector}}})).await?.data;
            verify_identity_store(&result, scope)?;
            identity::user_id(required(&result, "UserId")?).ok_or_else(|| {
                EnterpriseError::protocol("Identity Center returned an invalid user ID")
            })?
        };
        let result = self
            .transport
            .call(
                Service::IdentityStore,
                &scope.identity_center_region,
                "DescribeUser",
                json!({"IdentityStoreId":scope.identity_store_id,"UserId":id}),
            )
            .await?
            .data;
        verify_identity_store(&result, scope)?;
        let user = EnterpriseUser::parse(&result, claims)?;
        if user.user_id != id {
            return Err(EnterpriseError::protocol(
                "resolved user does not match the requested Identity Center user",
            ));
        }
        Ok(user)
    }

    #[allow(clippy::too_many_arguments)]
    async fn pages(
        &self,
        service: Service,
        region: &str,
        operation: &str,
        mut body: Value,
        collection: &str,
        token_key: &str,
    ) -> Result<Vec<Value>, EnterpriseError> {
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        let mut received_bytes = 0usize;
        for _ in 0..MAX_PAGES {
            let result = self
                .transport
                .call(service, region, operation, body.clone())
                .await?;
            received_bytes = received_bytes.saturating_add(result.payload_bytes);
            if received_bytes > MAX_COLLECTION_BYTES {
                return Err(EnterpriseError::protocol(
                    "AWS listing exceeded the aggregate byte limit; no partial list returned",
                ));
            }
            let result = result.data;
            let page = result
                .get(collection)
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    EnterpriseError::protocol("AWS listing is missing its expected array")
                })?;
            if items.len().saturating_add(page.len()) > MAX_ITEMS {
                return Err(EnterpriseError::protocol(
                    "AWS listing exceeded the item limit; no partial list returned",
                ));
            }
            items.extend(page.iter().cloned());
            match result.get(token_key) {
                None | Some(Value::Null) => return Ok(items),
                Some(Value::String(token)) if token.is_empty() => return Ok(items),
                Some(Value::String(token))
                    if token.len() <= 16384 && seen.insert(token.clone()) =>
                {
                    body[token_key] = json!(token);
                }
                _ => {
                    return Err(EnterpriseError::protocol(
                        "AWS pagination token is invalid or repeated; no partial list returned",
                    ))
                }
            }
        }
        Err(EnterpriseError::protocol(
            "AWS listing exceeded 100 pages; no partial list returned",
        ))
    }
}

fn validate_user_selector(value: &str) -> Result<(), EnterpriseError> {
    if value.is_empty()
        || value.chars().count() > 128
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || (value.starts_with("arn:") && identity::user_id(value).is_none())
    {
        return Err(EnterpriseError::invalid(
            "an exact Identity Center user ID, user ARN or userName of at most 128 characters is required",
        ));
    }
    Ok(())
}

fn verify_identity_store(value: &Value, scope: &EnterpriseScope) -> Result<(), EnterpriseError> {
    let reference = required(value, "IdentityStoreId")?;
    let actual = identity::store_id(reference).ok_or_else(|| {
        EnterpriseError::protocol("AWS response has an invalid identity store ID")
    })?;
    if actual != scope.identity_store_id
        || identity::store_account(reference).is_some_and(|account| {
            scope
                .identity_store_account
                .as_deref()
                .is_some_and(|expected| account != expected)
        })
    {
        return Err(EnterpriseError::protocol(
            "AWS response belongs to a different Identity Center store",
        ));
    }
    Ok(())
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, EnterpriseError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| EnterpriseError::protocol("AWS response is missing a required identifier"))
}

fn is_application_arn(value: &str) -> bool {
    value.starts_with("arn:aws:sso:") && value.contains(":application/")
}

#[cfg(test)]
fn is_user_id(value: &str) -> bool {
    identity::user_id(value).is_some()
}

#[cfg(test)]
mod tests;
