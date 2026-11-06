# Sign-in with loony-auth (proposal)

**Status: built, not live.** The server and web UI exist, and sign-in is off by default. It
has only run against a fake identity provider, not the real loony-auth. The work is broken down in
[projects-and-signin-todo.md](projects-and-signin-todo.md). For how credentials work today,
see [api-reference.md](api-reference.md) and [admin.md](admin.md).

## Problem

There is no self-signup today. To add a user, an admin creates a key in the admin panel
and sends it to them, and they paste it in to sign in. That key keeps working until
someone disables it.

The goal is for people to sign in with their Loony account, with no key to copy, and then
work in projects: the ones they created and the ones they've been invited to. Each project
has its own buckets, access keys and members.

loony-auth can prove who someone is, but every LS3 request must be signed with an access
key and secret (SigV4). A loony-auth token can't sign those requests, so LS3 has to swap
a proof of identity for a key.

## Proposed flow

```
Browser (web UI)           LS3 server                 loony-auth
      |                        |                          |
      |-- 1. Sign in with Loony (PKCE) ------------------>|
      |<-- 2. back with a one-time code ------------------|
      |-- 3. code + verifier ->|                          |
      |                        |-- 4. exchange code ----->|
      |                        |<--    tokens ------------|
      |                        | 5. check ID token,       |
      |                        |    find or create user,  |
      |                        |    mint session key      |
      |<-- 6. session key -----|                          |
      |-- 7. list projects --->|                          |
      |-- 8. open a project -->|                          |
      |<-- project key --------|                          |
      |-- 9. SigV4 requests -->|                          |
```

1. The user clicks **Sign in with Loony**. The page generates `state`, `nonce` and a PKCE
   verifier, then navigates to `https://auth.lovegood.in/oauth/authorize`.
2. The user logs in on loony-auth, which redirects back with a one-time code. The page
   checks `state`.
3. The page sends the code, verifier and nonce to a new public endpoint, such as
   `POST /_console/v1/session`. Bucket names can't start with `_`, so this path can't
   clash with one.
4. The LS3 server exchanges the code at `/oauth/token` as a confidential client. The
   client secret stays on the server.
5. The server verifies the ID token against loony-auth's JWKS: the algorithm pinned to
   EdDSA, plus issuer, audience, expiry and nonce. It then finds the user by the token's
   `iss` and `sub`, so a first sign-in creates the user record.
6. The server returns a temporary session key. It can call the console endpoints, such as
   listing projects, but owns no buckets. The page keeps it for the browser session.
7. The page lists the user's projects and invites.
8. When the user opens a project, the server checks their membership and returns a
   temporary key tied to that project.
9. The UI signs bucket and file requests with that key, exactly as it signs requests today.

The root key and the local-only admin panel don't change.

## Decisions

### The LS3 server exchanges the code, not the browser

The browser can't call loony-auth's token endpoint from the UI's origin. A live CORS
preflight against `/oauth/token` from `https://s3-console.lovegood.in` returned no
`access-control-allow-origin` header.

Adding the origin to `CORS_ALLOWED_ORIGINS` would fix that, but the same CORS policy has
`allow-credentials: true`, so it would also let that origin make cookie-authenticated
calls to auth-api. Exchanging on the server avoids this and means loony-auth needs no code
or CORS change, only a registered client.

Each code is single-use, so the swap endpoint doesn't accept a reusable token.

### Temporary keys last about 8 hours, with no refresh tokens

From reading loony-auth's code, not from testing:

- `set_user_status` (`services/admin-api/src/handlers.rs`) doesn't appear to revoke
  sessions or refresh tokens.
- The refresh path doesn't appear to check the user's status.
- Refresh tokens last 30 days by default (`REFRESH_TOKEN_TTL=2592000` in `.env.example`).

Renewing with a refresh token could therefore let a disabled user keep getting new keys.
Instead, when a key is near expiry, the page goes back through `/oauth/authorize`. While
the user's loony-auth session is active (7 days by default), this normally needs no
password.

Eight hours keeps redirects rare. The longer lifetime is acceptable because LS3 can cut
off a key immediately (see below).

### LS3 gets its own Disable user action

A new admin action ends a user's temporary keys and refuses them new ones. LS3 already
looks up the credential and checks `enabled` on every request, so this takes effect
immediately.

Disabling someone in loony-auth is not enough to lock them out of LS3.

### Projects own buckets and access keys

A project gets its own `OwnerId`, so LS3's existing ownership checks keep projects apart.
Access keys belong to the project, not a person, so an app keeps working when someone
leaves. Disabling a user therefore doesn't stop a project's access keys, and a departing
member may have seen one, so it should be rotated. An admin action to disable a project
stops all of its keys at once.

### People use their normal Loony account

Use loony-auth's existing `loony` organization, with invites handled per project inside
LS3. Anyone can have a Loony account, but they only see projects they created or were
invited to.

LS3 stores `quota_bytes` on each bucket but never enforces it, so open project creation
would let a stranger fill the disk. Only people an admin approves can create projects
until storage limits are enforced.

## Alternatives considered

| Option | Why not |
|---|---|
| Browser exchanges the code itself (public client) | Needs the CORS change described above |
| Create a permanent key at login | A stolen key stays useful until someone notices |
| LS3 session cookie instead of keys, as loony-library does | Adds cookie and cross-site protection to the S3 API, and replaces the UI's request signing |
| Keep creating keys by hand | Admin work for every user, and each person still has to paste keys in |

## Known limitations

- **Share links:** a presigned URL made with a temporary key stops working when that key
  expires, so links from the UI would last 8 hours at most. Longer-lasting links need a
  permanent key.
- **Long uploads:** a multipart upload still running when the key expires could fail. The
  page should renew the key before starting a big upload, not interrupt one partway
  through.
- **Secret exposure:** a script injected into the UI could use the temporary key while
  it's valid. This is still better than today, where the root secret sits in
  `localStorage` until sign-out.

## Open questions

- **Does disabling a loony-auth user really leave refresh tokens working?** This needs
  testing. If confirmed, it's a bug to fix in loony-auth, and it also affects other apps
  that use loony-auth, including loony-library.
