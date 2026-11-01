# IAM authentication

[Back to README](../README.md) · [Python SDK](python.md) · [Operations](operations.md)

The optimizer validates JWT access tokens issued by one trusted OIDC provider.
Zitadel and Keycloak both expose OIDC discovery and public signing keys. Clients
obtain their tokens through the provider and send `authorization: Bearer TOKEN`
metadata on every RPC. The server needs no IAM client secret.

## Enable authentication

Set these values in `.env` for `just` / Compose, or export them when launching
the binary directly:

```dotenv
OPT_AUTH_ENABLED=true
OPT_AUTH_ISSUER=https://iam.example.com/realms/optimizer
OPT_AUTH_AUDIENCE=grpc-optimizer
```

Start with `just run` or `docker compose up -d`. For a locally built image, pass
the settings with `just docker-run --env-file .env -d`. Keep the listener behind
a TLS-terminating gRPC ingress and forward authorization metadata unchanged.

| Variable | Default | Purpose |
| --- | --- | --- |
| `OPT_AUTH_ENABLED` | `false` | Require tokens on optimizer and reflection RPCs |
| `OPT_AUTH_ISSUER` | Required when enabled | Exact token issuer, including any realm path |
| `OPT_AUTH_AUDIENCE` | Required when enabled | API identifier expected in `aud` |
| `OPT_AUTH_ALGORITHMS` | `RS256` | Comma-separated allowed asymmetric signing algorithms |
| `OPT_AUTH_ALLOW_INSECURE_HTTP` | `false` | Permit HTTP discovery/JWKS for local IAM development |

Supported algorithms are `RS256`, `RS384`, `RS512`, `PS256`, `PS384`, `PS512`,
`ES256`, `ES384`, and `EdDSA`. Select the algorithm actually used by the provider.
Shared-secret algorithms are rejected. The HTTP development override does not
disable HTTPS certificate validation.

Enabling authentication requires successful discovery at
`ISSUER/.well-known/openid-configuration` and initial signing-key retrieval.
Incomplete settings, issuer mismatch, and unusable keys prevent startup.
Discovery and JWKS URLs require HTTPS by default and cannot contain credentials,
query parameters, or fragments. Redirects are not followed.

## Keycloak

1. Create a realm, for example `optimizer`, with RS256 signing enabled.
2. Register an API client identifier such as `grpc-optimizer`. Use that value as
   `OPT_AUTH_AUDIENCE` and the realm issuer as `OPT_AUTH_ISSUER`, for example
   `https://keycloak.example.com/realms/optimizer`.
3. For a machine client, enable client authentication and service accounts.
   Add an audience protocol mapper to its assigned client scope, include
   `grpc-optimizer` in access-token audiences, and ensure that scope is applied
   to the requesting client. A login client ID alone does not automatically
   establish the API audience.
4. Request an access token using the machine client's credentials:

```sh
export OPT_ACCESS_TOKEN="$(curl --fail --silent --show-error \
  "$OPT_AUTH_ISSUER/protocol/openid-connect/token" \
  --data-urlencode grant_type=client_credentials \
  --data-urlencode "client_id=$IAM_CLIENT_ID" \
  --data-urlencode "client_secret=$IAM_CLIENT_SECRET" \
  | jq -er .access_token)"
```

For people, use an OIDC client with authorization-code/PKCE or device login and
the same API audience mapping. Send the resulting access token. Keep login-client
audiences distinct from the API audience so ID tokens are rejected by audience
validation. See Keycloak's [OIDC endpoints](https://www.keycloak.org/securing-apps/oidc-layers)
and [audience configuration](https://www.keycloak.org/docs/latest/server_admin/#_audience).

## Zitadel

1. Create an optimizer project and use its project ID as `OPT_AUTH_AUDIENCE`.
   Set `OPT_AUTH_ISSUER` to the instance's exact issuer, such as
   `https://tenant.zitadel.cloud`.
2. Create a service account. In its general settings, set its access-token type
   to **JWT**; the service-account default is opaque. Generate its client secret.
3. Request a token with the project's audience scope:

```sh
export OPT_ACCESS_TOKEN="$(curl --fail --silent --show-error \
  "$OPT_AUTH_ISSUER/oauth/v2/token" \
  --data-urlencode grant_type=client_credentials \
  --data-urlencode "client_id=$IAM_CLIENT_ID" \
  --data-urlencode "client_secret=$IAM_CLIENT_SECRET" \
  --data-urlencode "scope=openid urn:zitadel:iam:org:project:id:$OPT_AUTH_AUDIENCE:aud" \
  | jq -er .access_token)"
```

For interactive users, configure the OIDC application's access-token type as JWT
and request the same project audience scope during your existing login flow.
Opaque tokens and personal access tokens require introspection, which this
server does not implement. A private-key JWT used to authenticate to Zitadel's
token endpoint is an assertion; send the returned access token to the optimizer.
See Zitadel's [service-account client credentials](https://zitadel.com/docs/guides/integrate/service-accounts/client-credentials)
and [OAuth endpoints](https://zitadel.com/docs/apis/openidoauth/endpoints).

## Call the optimizer

With `OPT_ACCESS_TOKEN` exported, use a TLS ingress:

```sh
just client --endpoint https://optimizer.example.com:443 --jobs 1
just client --endpoint https://optimizer.example.com:443 --mode session
just bench --endpoint https://optimizer.example.com:443 --mode unary --jobs 10
just python-client --endpoint optimizer.example.com:443 --tls
just python-pyomo-client --endpoint optimizer.example.com:443 --tls
```

Both example languages accept `--access-token` as well. Python SDK calls accept
an explicit `access_token=` and `tls=True`; see the [SDK guide](python.md#tls-and-channel-ownership).
Health `Check` and `Watch` remain public, so `just client --health` and container
health checks continue to work. Tools using reflection must supply the token,
for example `grpcurl -H "authorization: Bearer $OPT_ACCESS_TOKEN" ...`.

## Verification and lifetime

The verifier requires a public-key signature, configured issuer, matching
audience, nonempty subject, and expiration. It validates `nbf` when supplied and
allows 30 seconds of clock skew. Tokens are limited to 16 KiB, and discovery/JWKS
documents to 1 MiB. HTTP requests have a five-second timeout.

Signing keys are cached for five minutes. Expired caches and unknown key IDs
trigger a shared refresh, with at most one attempt every 30 seconds, including
the initial fetch. New key IDs may therefore take up to 30 seconds to become
usable. Publish old and new keys with an overlap when rotating. A successful
refresh replaces the key set, removing withdrawn keys.
An in-flight refresh completes even if the initiating RPC is cancelled or times
out; other callers share its result while respecting their own deadlines.

Fresh known keys remain usable during provider outages. Requests needing an
unavailable refresh receive `UNAVAILABLE`; successful refreshes with an unknown
key ID produce `UNAUTHENTICATED`. Expired caches are never used for verification.

Authentication runs once per RPC. An accepted stream continues until it closes
or reaches its deadline, even if its token expires. Obtain a fresh token for
new calls. Clients do not refresh or retry tokens automatically, and local JWT
verification does not check revocation online.

Every token passing these checks can use all optimizer RPCs. This version does
not enforce roles or scopes: audience identifies the intended API, not the
caller's project permissions. Configure your IAM access and token issuance
policies accordingly. Credentials and provider response bodies are excluded from
authentication errors and application logs.

## Development checks

```sh
just check
just python-check
just auth-smoke
```

`auth-smoke` needs `just python-setup` first. It runs the Python SDK against a
local gRPC server and a mock OIDC provider with public test signing keys. No IAM
credentials or separately running server are needed. CI runs the same check.
