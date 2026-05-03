# Configuration

The adapter context that owns *typed configuration*. Values come from code
defaults, then `~/.config/agentry/agentry.toml`, then environment variables
prefixed `AGENTRY_` with `__` section delimiters. Load order is
later-overrides-earlier. This context produces configuration values for
every other context; it consumes none.

## Config

The root configuration record: Redis, dashboard, signing, webhook, forge
sections. Loaded at binary startup; held immutably for the process lifetime.

## RedisConfig

Redis endpoint and credentials. Default is the local Podman dev Redis
(`redis://127.0.0.1:6380`). A deployment-level knob; never hard-coded to a
production target.

## DashboardConfig

Dashboard HTTP port. Default `7800`.

## SigningConfig

Path to the ed25519 signing key used by the permits context to sign
`WorkPermit`s at mint time.

## WebhookConfig

Dashboard webhook trigger secret. When unset, `POST /submit` is disabled
(401). When set, the secret must appear in the `X-Agentry-Token` request
header.

## ForgeConfig

Forge defaults applied when a brief's payload does not carry its own
`forge_host`. The `default_host` field is `host:port` (no scheme); when set
it is combined with the brief's `target_repo` to construct the token-bearing
clone URL. When neither the payload nor this default is set, brief dispatch
fails with a clear configuration error rather than falling back to a
hardcoded literal.
