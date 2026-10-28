# MiMo Code Token Plan quota (research, 2026-10-28)

Mjolnir reports MiMo Code quota as unavailable
(`HarnessKind::Mimo => Ok(not_reported())` in `mj-controller/src/quota.rs`).
This note records why, and what a future integration would need.

## Result

The Xiaomi Token Plan's remaining credits cannot be read with the API key that
MiMo Code stores. Only a logged-in web console session can read them, through
undocumented console endpoints.

## Credential MiMo Code holds

`$MIMOCODE_HOME/data/auth.json` (by default `~/.local/share/mimocode/auth.json`)
has this shape:

```json
{ "xiaomi": { "type": "api", "key": "tp-…",
              "metadata": { "uid": "…", "base_url": "https://token-plan-ams.xiaomimimo.com/v1" } } }
```

The regional inference hosts are `token-plan-{ams,sgp,cn}.xiaomimimo.com/v1`.
The pay-as-you-go host is `api.xiaomimimo.com/v1`.

## What the inference host exposes

The inference host serves only the OpenAI-compatible `/v1/*` routes.

- `GET /v1/models` returns 200 with a model list. Its response headers carry no
  quota information.
- These paths all return an openresty 404 with the `tp-` bearer key:
  `/v1/usage`, `/v1/balance`, `/v1/user/balance`, `/v1/quota`, `/v1/credits`,
  `/v1/subscription`, `/v1/plan`, `/v1/token-plan`, `/v1/account`, `/v1/me`,
  `/v1/dashboard/billing/subscription`, `/v1/dashboard/billing/usage`,
  `/api/v1/usage` and `/api/v1/balance`.
- A real `chat/completions` call returns only token counts in its `usage`
  field. It sends no `x-ratelimit-*` or credit headers.
- The MiMo Code binary (checked at its 2026-10-26 build) contains no usage or
  balance endpoint. It only links users to `platform.xiaomimimo.com`.

## Console endpoints (undocumented)

These endpoints live on `https://platform.xiaomimimo.com` and back the
`/console/plan-manage` page:

| Request | Returns |
|---|---|
| `GET /api/v1/tokenPlan/usage` | `monthUsage` and `usage`, each with `items[]` of `name`, `used`, `limit` and `percent` |
| `GET /api/v1/tokenPlan/detail` | `planCode`, `planName`, `currentPeriodEnd`, `expired` and `enableAutoRenew` |
| `POST /api/v1/usage/token-plan/list` with body `{year, month}` | per-day and per-model `totalToken`, `inputHitToken`, `inputMissToken`, `outputToken` and `requestCount` |

The endpoints authenticate with console cookies: `api-platform_serviceToken`,
`userId`, and optionally `api-platform_ph`. The `tp-` key does not work here.
An unauthenticated request returns 401 with
`{"code":401,"loginUrl":"https://account.xiaomi.com/pass/serviceLogin?…&sid=api-platform"}`.
The 401 was verified live on 2026-10-28. The response fields come from
community reverse engineering, linked below, and have not been verified with a
real session. The cookie lifetime is unknown.

## Integration options

- **Console cookie (viable).** Store the cookies on the MiMo profile and have
  the quota poller read `/api/v1/tokenPlan/usage`. Report the monthly credits
  as a window with used, limit and percent. Use `currentPeriodEnd` as the reset
  time. When the cookie expires, the 401 must surface as an expired console
  session that says how to refresh it. Automating the Xiaomi account login
  (password and 2FA) would be fragile and is not recommended.
- **Local accounting (rejected).** Summing tokens from MiMo's session database
  against the plan size gives a confident wrong number. Credits are not
  tokens: the rate varies by model, and usage from 00:00 to 08:00 Beijing time
  counts at 0.8x. Other clients on the same plan would also be invisible.

Plan sizes per month: Lite 4.1B credits, Standard 11B, Pro 38B, Max 82B. When
the credits run out, the service stops; it does not charge the account balance.

## References

- Feature request for an API-key usage endpoint, open with no maintainer reply:
  https://github.com/XiaomiMiMo/MiMo-Code/issues/2495
- Console endpoint and cookie details:
  https://github.com/diegosouzapw/OmniRoute/issues/15753
- Plan sizes and the off-peak rate:
  https://mimo.mi.com/docs/en-US/tokenplan/Token%20Plan/subscription
