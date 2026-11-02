//! The Console protocols have no public SDK model. All requests below are
//! POSTs to a fixed regional root, signed over the exact bytes sent on the wire.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use quick_xml::{events::Event, Reader};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::types::{validate_region, AwsCredentials, EnterpriseError, WriteOutcome};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Service {
    Profiles,
    Assignments,
    Sso,
    IdentityStore,
    Subscriptions,
    Sts,
}

pub(super) struct AwsTransport {
    credentials: AwsCredentials,
    client: reqwest::Client,
    pub(super) region: String,
    pub(super) legacy: bool,
    deadline: Instant,
    #[cfg(test)]
    pub(super) test_endpoint: Option<String>,
}

pub(super) struct RpcResult {
    pub data: Value,
    pub request_id: Option<String>,
    pub payload_bytes: usize,
}

struct RpcEndpoint {
    host: String,
    signing_name: &'static str,
    prefix: &'static str,
    content_type: &'static str,
}

impl AwsTransport {
    pub(super) fn new(
        credentials: AwsCredentials,
        region: String,
        legacy: bool,
    ) -> Result<Self, EnterpriseError> {
        validate_region(&region)?;
        if legacy && region != "us-east-1" {
            return Err(EnterpriseError::invalid(
                "legacy control plane requires us-east-1",
            ));
        }
        let client = reqwest::Client::builder()
            .https_only(true)
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(40))
            .build()
            .map_err(|_| {
                EnterpriseError::new(
                    "client_unavailable",
                    503,
                    "could not initialize AWS transport",
                )
            })?;
        Ok(Self {
            credentials,
            client,
            region,
            legacy,
            deadline: Instant::now() + Duration::from_secs(120),
            #[cfg(test)]
            test_endpoint: None,
        })
    }

    #[cfg(test)]
    pub(super) fn with_test_endpoint(mut self, endpoint: String) -> Self {
        self.client = reqwest::Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        self.test_endpoint = Some(endpoint);
        self
    }

    pub(super) async fn call(
        &self,
        service: Service,
        region: &str,
        operation: &str,
        body: Value,
    ) -> Result<RpcResult, EnterpriseError> {
        let endpoint = self.endpoint(service, region);
        let target = format!("{}.{operation}", endpoint.prefix);
        let body = serde_json::to_vec(&body)
            .map_err(|_| EnterpriseError::invalid("could not encode AWS request"))?;
        self.request(
            &endpoint,
            region,
            Some(&target),
            body,
            matches!(service, Service::Assignments),
        )
        .await
    }

    pub(super) async fn caller_account(&self) -> Result<String, EnterpriseError> {
        let endpoint = self.endpoint(Service::Sts, &self.region);
        let result = self
            .request(
                &endpoint,
                &self.region,
                None,
                b"Action=GetCallerIdentity&Version=2011-06-15".to_vec(),
                false,
            )
            .await?;
        result
            .data
            .get("Account")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| EnterpriseError::protocol("STS response is missing the AWS account"))
    }

    fn endpoint(&self, service: Service, region: &str) -> RpcEndpoint {
        let kiro_host = || {
            if self.legacy {
                "codewhisperer.us-east-1.amazonaws.com".to_owned()
            } else {
                format!("management.{region}.kiro.dev")
            }
        };
        let (host, signing_name, prefix, content_type) = match service {
            Service::Profiles => (
                kiro_host(),
                "codewhisperer",
                if self.legacy {
                    "AWSCodeWhispererService"
                } else {
                    "KiroControlPlaneService"
                },
                "application/x-amz-json-1.0",
            ),
            Service::Assignments => (
                kiro_host(),
                "q",
                if self.legacy {
                    "AmazonQDeveloperService"
                } else {
                    "KiroControlPlaneQDeveloperService"
                },
                "application/x-amz-json-1.0",
            ),
            Service::Sso => (
                format!("sso.{region}.amazonaws.com"),
                "sso",
                "SWBExternalService",
                "application/x-amz-json-1.1",
            ),
            Service::IdentityStore => (
                format!("identitystore.{region}.amazonaws.com"),
                "identitystore",
                "AWSIdentityStore",
                "application/x-amz-json-1.1",
            ),
            Service::Subscriptions => (
                format!("service.user-subscriptions.{region}.amazonaws.com"),
                "user-subscriptions",
                "AWSZornControlPlaneService",
                "application/x-amz-json-1.0",
            ),
            Service::Sts => (
                format!("sts.{region}.amazonaws.com"),
                "sts",
                "",
                "application/x-www-form-urlencoded",
            ),
        };
        RpcEndpoint {
            host,
            signing_name,
            prefix,
            content_type,
        }
    }

    async fn request(
        &self,
        endpoint: &RpcEndpoint,
        region: &str,
        target: Option<&str>,
        body: Vec<u8>,
        write: bool,
    ) -> Result<RpcResult, EnterpriseError> {
        let host = &endpoint.host;
        let service = endpoint.signing_name;
        let content_type = endpoint.content_type;
        let operation = target.unwrap_or("GetCallerIdentity");
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(EnterpriseError::new(
                "aws_timeout",
                504,
                "AWS discovery deadline exceeded; no further request was submitted",
            ));
        }
        let now = time::OffsetDateTime::now_utc();
        let timestamp = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            now.year(),
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
        let headers = sign(
            &self.credentials,
            host,
            service,
            region,
            content_type,
            target,
            &body,
            &timestamp,
        )?;
        #[allow(unused_mut)]
        let mut url = format!("https://{host}/");
        #[cfg(test)]
        if let Some(endpoint) = &self.test_endpoint {
            url = format!("{endpoint}/");
        }

        // One submission only: no redirects, no application-level retries and
        // no fallback to another region, profile, endpoint or credential chain.
        let mut response = self
            .client
            .post(url)
            .headers(headers)
            .body(body)
            .timeout(remaining.min(Duration::from_secs(40)))
            .send()
            .await
            .map_err(|error| request_error(operation, write, error.is_timeout()))?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .or_else(|| response.headers().get("x-amz-request-id"))
            .and_then(|value| value.to_str().ok())
            .filter(|value| self.safe_identifier(value))
            .map(str::to_owned);
        let mut payload = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            let mut error = request_error(operation, write, error.is_timeout());
            error.upstream_status = Some(status.as_u16());
            error.upstream_request_id = request_id.clone();
            error
        })? {
            if payload.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(response_error(
                    operation,
                    write,
                    status.as_u16(),
                    request_id,
                    "AWS response exceeded the size limit",
                ));
            }
            payload.extend_from_slice(&chunk);
        }
        let json = if service == "sts" {
            None
        } else {
            serde_json::from_slice::<Value>(&payload).ok()
        };
        let xml_code = if service == "sts" && !status.is_success() {
            xml_field(&payload, &[b"ErrorResponse", b"Error", b"Code"]).ok()
        } else {
            None
        };
        let code = json
            .as_ref()
            .and_then(|data| {
                data.get("__type")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .or_else(|| {
                        data.get("code")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                    })
            })
            .or(xml_code.as_deref())
            .map(|value| {
                value
                    .rsplit('#')
                    .next()
                    .unwrap_or(value)
                    .split(':')
                    .next()
                    .unwrap_or(value)
            });
        let error_envelope = json
            .as_ref()
            .is_some_and(|data| data.get("__type").is_some() || data.get("code").is_some());
        if !status.is_success() || error_envelope {
            let code = code.or_else(|| {
                if service == "sts" {
                    None
                } else {
                    Some("UnrecognizedUpstreamError")
                }
            });
            let (http_status, public_code) = match code.unwrap_or("") {
                "AccessDeniedException"
                | "AccessDenied"
                | "UnauthorizedException"
                | "UnrecognizedClientException"
                | "InvalidClientTokenId"
                | "ExpiredTokenException"
                | "ExpiredToken"
                | "InvalidSignatureException"
                | "SignatureDoesNotMatch" => (403, "aws_access_denied"),
                "ResourceNotFoundException" | "ResourceNotFound" => (404, "not_found"),
                "ConflictException" | "ResourceAlreadyExistsException" => (409, "conflict"),
                "ThrottlingException" | "TooManyRequestsException" => (429, "aws_throttled"),
                "ValidationException" | "ValidationError" => (400, "invalid_request"),
                _ if matches!(status.as_u16(), 401 | 403) => (403, "aws_access_denied"),
                _ if status.as_u16() == 429 => (429, "aws_throttled"),
                _ => (502, "aws_request_failed"),
            };
            let definitive_rejection = matches!(
                public_code,
                "aws_access_denied"
                    | "not_found"
                    | "conflict"
                    | "aws_throttled"
                    | "invalid_request"
            );
            let uncertain = write
                && (status.is_server_error()
                    || status.is_redirection()
                    || status.as_u16() == 408
                    || (status.is_success() && !definitive_rejection));
            let mut error = EnterpriseError::new(
                public_code,
                http_status,
                if uncertain {
                    "AWS write outcome is uncertain; query subscription state before retrying"
                } else {
                    "AWS rejected the request; inspect the upstream code and request ID"
                },
            );
            error.operation = Some(operation.into());
            error.upstream_status = Some(status.as_u16());
            error.upstream_request_id = request_id;
            error.upstream_code = code.map(known_error_code).map(str::to_owned);
            error.write_outcome = if !write {
                WriteOutcome::NotSubmitted
            } else if uncertain {
                WriteOutcome::Unknown
            } else {
                WriteOutcome::Rejected
            };
            return Err(error);
        }
        let data = if service == "sts" {
            let account = sts_account(&payload).map_err(|_| {
                response_error(
                    operation,
                    write,
                    status.as_u16(),
                    request_id.clone(),
                    "invalid STS identity response",
                )
            })?;
            serde_json::json!({"Account":account})
        } else if payload.is_empty() {
            serde_json::json!({})
        } else {
            json.filter(Value::is_object).ok_or_else(|| {
                response_error(
                    operation,
                    write,
                    status.as_u16(),
                    request_id.clone(),
                    "AWS response is not a JSON object",
                )
            })?
        };
        Ok(RpcResult {
            data,
            request_id,
            payload_bytes: payload.len(),
        })
    }

    fn safe_identifier(&self, value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            && !value.contains(self.credentials.access_key_id.as_str())
            && !value.contains(self.credentials.secret_access_key.as_str())
            && self
                .credentials
                .session_token
                .as_ref()
                .is_none_or(|token| !value.contains(token.as_str()))
    }
}

fn known_error_code(code: &str) -> &'static str {
    match code {
        "AccessDeniedException" => "AccessDeniedException",
        "AccessDenied" => "AccessDenied",
        "UnauthorizedException" => "UnauthorizedException",
        "UnrecognizedClientException" => "UnrecognizedClientException",
        "InvalidClientTokenId" => "InvalidClientTokenId",
        "ExpiredTokenException" => "ExpiredTokenException",
        "ExpiredToken" => "ExpiredToken",
        "InvalidSignatureException" => "InvalidSignatureException",
        "SignatureDoesNotMatch" => "SignatureDoesNotMatch",
        "ResourceNotFoundException" => "ResourceNotFoundException",
        "ResourceNotFound" => "ResourceNotFound",
        "ConflictException" => "ConflictException",
        "ResourceAlreadyExistsException" => "ResourceAlreadyExistsException",
        "ThrottlingException" => "ThrottlingException",
        "TooManyRequestsException" => "TooManyRequestsException",
        "ValidationException" => "ValidationException",
        "ValidationError" => "ValidationError",
        "InternalServerException" => "InternalServerException",
        "InternalServerError" => "InternalServerError",
        "ServiceQuotaExceededException" => "ServiceQuotaExceededException",
        "UnknownOperationException" => "UnknownOperationException",
        _ => "UnrecognizedUpstreamError",
    }
}

fn request_error(operation: &str, write: bool, timeout: bool) -> EnterpriseError {
    let mut error = EnterpriseError::new(
        if timeout {
            "aws_timeout"
        } else {
            "aws_transport_error"
        },
        if timeout { 504 } else { 502 },
        "AWS request did not complete; query subscription state before retrying a write",
    );
    error.operation = Some(operation.into());
    if write {
        error.write_outcome = WriteOutcome::Unknown;
    }
    error
}

fn response_error(
    operation: &str,
    write: bool,
    status: u16,
    request_id: Option<String>,
    message: &'static str,
) -> EnterpriseError {
    let mut error = EnterpriseError::protocol(message);
    error.operation = Some(operation.into());
    error.upstream_status = Some(status);
    error.upstream_request_id = request_id;
    if write {
        error.write_outcome = WriteOutcome::Unknown;
    }
    error
}

fn sts_account(payload: &[u8]) -> Result<String, ()> {
    let account = xml_field(
        payload,
        &[
            b"GetCallerIdentityResponse",
            b"GetCallerIdentityResult",
            b"Account",
        ],
    )?;
    if account.len() != 12 || !account.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    Ok(account)
}

fn xml_field(payload: &[u8], expected_path: &[&[u8]]) -> Result<String, ()> {
    let mut reader = Reader::from_reader(payload);
    reader.config_mut().trim_text(true);
    let mut path = Vec::new();
    let mut value = None;
    let mut root_seen = false;
    loop {
        match reader.read_event().map_err(|_| ())? {
            Event::Start(event) => {
                if path.len() >= 32 {
                    return Err(());
                }
                if path.is_empty() {
                    if root_seen {
                        return Err(());
                    }
                    root_seen = true;
                }
                path.push(event.local_name().as_ref().to_vec());
            }
            Event::Empty(_) if path.is_empty() => {
                if root_seen {
                    return Err(());
                }
                root_seen = true;
            }
            Event::End(_) => {
                path.pop().ok_or(())?;
            }
            Event::Text(event)
                if path
                    .iter()
                    .map(Vec::as_slice)
                    .eq(expected_path.iter().copied()) =>
            {
                if value.is_some() {
                    return Err(());
                }
                let text = event.unescape().map_err(|_| ())?.into_owned();
                if text.len() > 256 {
                    return Err(());
                }
                value = Some(text);
            }
            Event::Text(event) if path.is_empty() && !event.is_empty() => return Err(()),
            Event::DocType(_) => return Err(()),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || !path.is_empty() {
        return Err(());
    }
    value.ok_or(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sign(
    credentials: &AwsCredentials,
    host: &str,
    service: &str,
    region: &str,
    content_type: &str,
    target: Option<&str>,
    body: &[u8],
    timestamp: &str,
) -> Result<HeaderMap, EnterpriseError> {
    let mut headers = BTreeMap::from([
        ("content-type", content_type.to_owned()),
        ("host", host.to_owned()),
        ("x-amz-date", timestamp.to_owned()),
    ]);
    if let Some(target) = target {
        headers.insert("x-amz-target", target.to_owned());
    }
    if let Some(token) = &credentials.session_token {
        headers.insert("x-amz-security-token", token.to_string());
    }
    let signed_headers = headers.keys().copied().collect::<Vec<_>>().join(";");
    let canonical_headers = headers
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}:{}\n",
                value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect::<String>();
    let canonical = format!(
        "POST\n/\n\n{canonical_headers}\n{signed_headers}\n{:x}",
        Sha256::digest(body)
    );
    let date = &timestamp[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{:x}",
        Sha256::digest(canonical.as_bytes())
    );
    let secret = Zeroizing::new(format!("AWS4{}", credentials.secret_access_key.as_str()));
    let key = Zeroizing::new(hmac(secret.as_bytes(), date.as_bytes()));
    let key = Zeroizing::new(hmac(key.as_slice(), region.as_bytes()));
    let key = Zeroizing::new(hmac(key.as_slice(), service.as_bytes()));
    let key = Zeroizing::new(hmac(key.as_slice(), b"aws4_request"));
    let signature = hmac(key.as_slice(), string_to_sign.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    headers.insert("authorization", format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", credentials.access_key_id.as_str()));
    let mut result = HeaderMap::new();
    for (key, value) in headers {
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| EnterpriseError::invalid("invalid AWS credential header"))?;
        value.set_sensitive(matches!(key, "authorization" | "x-amz-security-token"));
        result.insert(HeaderName::from_static(key), value);
    }
    Ok(result)
}

fn hmac(key: &[u8], value: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key sizes");
    mac.update(value);
    mac.finalize().into_bytes().into()
}
