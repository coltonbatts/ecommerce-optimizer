# Publishing to Etsy (not implemented)

This tool reads market data and writes listings **locally**. It does not submit
anything to Etsy. This file records what that would actually take, so the next
person doesn't rediscover it.

## Why it's a separate project

The API key proves *which app* is calling. Publishing proves *which seller*
authorized it. Those are different mechanisms:

| | API key (implemented) | OAuth 2.0 (needed for writes) |
|---|---|---|
| Header | `x-api-key: keystring:shared_secret` | `Authorization: Bearer <token>` |
| Per seller? | No — one key per app | Yes — one token per shop |
| Setup | Copy two strings | Redirect URI, PKCE, consent screen, token storage |
| Expiry | Never | Access token expires; refresh token rotates |

Read-only public endpoints (`/v3/application/listings/active`) work with the key
alone. Anything touching private shop data or performing a write needs a token.

## The flow, as documented by Etsy

Verified from Etsy's published auth documentation, **not** verified by us against
a live call:

1. **Generate a PKCE verifier** — random per request. `code_challenge` is its
   SHA-256 image.
2. **Send the seller to consent:**
   `GET https://www.etsy.com/oauth/connect`
   with `response_type=code`, `client_id=<keystring>`, `redirect_uri=<https URL>`,
   `scope=<space separated>`, `code_challenge`, `code_challenge_method=S256`.
   The redirect URI must use `https://` or the request fails.
3. **Exchange the returned code:**
   `POST https://openapi.etsy.com/v3/public/oauth/token`
   with `grant_type=authorization_code`.
4. **Store the access and refresh tokens.** Refresh with
   `grant_type=refresh_token` against the same endpoint.

Scopes are granular: `listings_r`, `listings_w`, `shops_r`, `transactions_r`,
and so on. Request the minimum needed.

## What a seller app can do

An Etsy *seller app* is scoped to its own shop. That means redirect URIs and
token storage can be simple — a local loopback listener (`http://localhost:<port>`)
is the usual pattern for a desktop tool, though note Etsy requires `https://` on
the redirect URI, so a local handler needs a hosted hop or a tunnel.

## Unknowns — check before building

- Whether the `https://` redirect requirement can be satisfied by a loopback
  listener for a desktop app, or whether a small hosted callback is required.
- The exact `createDraftListing` payload requirements. Etsy validates price
  against a taxonomy `price` object and requires `who_made` / `when_made` /
  `is_supply`. None of that is modelled here.
- Rate limits under sustained write traffic.

Do not assume the local listing shape maps 1:1 onto the API's. It does not.

## Recommendation

Keep this out of the core tool. The value here is market intelligence and
listing *drafts*; publishing is an integration with a different failure surface
(auth, consent, token rotation) and belongs in its own crate or a separate tool
that consumes the SQLite database this one produces.