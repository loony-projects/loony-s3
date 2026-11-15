# Sign-in with loony-auth

**Status: built, not live.** Sign-in is off unless it's configured. It's tested end to end
against a fake identity provider; testing against the real loony-auth waits for LS3 to be
registered there (see [Registering LS3 in loony-auth](#registering-ls3-in-loony-auth)). The
work is broken down in [projects-and-signin-todo.md](projects-and-signin-todo.md). For how
credentials work, see [api-reference.md](api-reference.md) and [admin.md](admin.md).

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

## How it works

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
4. The LS3 server exchanges the code at `/oauth/token` as a confidential client
   (`client_secret_basic`), through `LS3_OIDC_INTERNAL_URL` when it's set. The client
   secret stays on the server. If loony-auth also returns a refresh token, the server
   revokes it at `/oauth/revoke` straight away and never stores it.
5. The server verifies the ID token against loony-auth's JWKS: the algorithm pinned to
   EdDSA, plus issuer, audience, expiry and nonce. The issuer must be the public one
   (`https://auth.lovegood.in`), even when the token came through the internal address.
   An unknown key ID refetches the JWKS, at most every 30 seconds. The server then finds
   the user by the token's `iss` and `sub`, never by email, so a first sign-in creates the
   user record. Signing in alone gives nobody anything: they see only projects they belong
   to and invites to their **verified** email address.
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

When a key is near expiry, the page goes back through `/oauth/authorize`. While the
person's loony-auth session is active (7 days), this needs no password. LS3 never keeps a
refresh token: it revokes any it's given, so there's nothing long-lived to steal.

Eight hours keeps redirects rare. The longer lifetime is acceptable because LS3 can cut
off a key immediately (see below).

### Cancelling isn't a failure

Pressing Cancel on loony-auth's consent screen comes back with `error=access_denied` and
the page's `state`. The page then shows the sign-in page again with "Sign-in was
cancelled." Any other error, or a `state` the page didn't send, gets an error message
instead, and the pending attempt is thrown away either way.

If LS3 is registered but not yet usable by someone, loony-auth shows its own error page
and doesn't come back, so there's nothing for LS3 to handle. The same goes for sign-up:
**Create account** sends `prompt=create`, and loony-auth's own rules and messages apply,
such as only `@lovegood.in` addresses for new accounts today. LS3 adds no checks of its own.

### Signing out ends both sessions

**Sign out** deletes the session key, and the project keys opened from it, then sends the
browser to `/oauth/logout?client_id=…&post_logout_redirect_uri=<this site>/login`. That
address shares an origin with the registered callback, which loony-auth requires, so it
returns to LS3's sign-in page with its own session ended too.

### Settings come from the server

The page reads the issuer, client ID, API address and region from `/config.json` when it
starts. loony-server serves that from its own settings (`LS3_OIDC_ISSUER`,
`LS3_OIDC_CLIENT_ID`, `LS3_PUBLIC_API_URL`, `LS3_REGION`, `LS3_ADMIN_ADDR`), and
`./loony ui` serves the same from the settings file, so everything is set once
in `.env` and one build works anywhere. The client secret is never in it.

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

Anyone can create projects, up to 10 each, but a project stores nothing until an Owner
pays for it, and then only up to its plan's storage. That's what keeps a stranger from
filling the disk. See [usage.md](usage.md#plans-and-billing).

### Deleted Loony accounts

People can delete their Loony account, and its `sub` never signs in again. LS3 isn't told,
so nothing happens automatically:

- Their projects keep running, and the projects' access keys keep working, until the
  subscription can't be paid. A subscription is paid from its payer's credit, which stays
  in LS3, so it runs until that's spent, then goes through grace, read-only and suspended
  as usual. Nothing is deleted.
- If they were the only Owner, nobody can manage the project. An admin can make someone
  else an Owner from the admin panel's **Projects** page (**Add Owner**), for example a
  colleague who already uses it. That's audited.
- Disabling the user in the admin panel ends any session they still have.

### Re-linking users after a loony-auth reset

Resetting loony-auth's database gives everyone a new `sub`. LS3 keys users by `iss` + `sub`,
so their next sign-in creates a second, empty user, and their projects stay with the old
one. LS3 never relinks on its own at sign-in, since a matching email alone mustn't be
enough to take over an account.

Instead, the admin panel's **Users** page lists pairs to re-link, one per person, when:

- exactly two users share an email address, and both addresses are **verified**;
- the newer user was created after the older one last signed in, so they aren't two
  accounts in use at once.

Anything ambiguous, such as three users with one address, isn't offered. An admin presses
**Re-link** for each pair. The old user then takes the new `sub`, along with everything the
new one did meanwhile: memberships (keeping the higher role), projects and invites it
made, access keys, credit (as a ledger entry), gift code redemptions, checkouts and
subscriptions it pays. The duplicate is deleted and its browser sessions end, so the
person signs in once more. Each re-link is one Raft command, checked again when applied,
and audited.

## Registering LS3 in loony-auth

In loony-auth's web app, under **All organizations › Loony › Apps › New app**:

| Field | Value |
| --- | --- |
| Type | Confidential (server) |
| First-party | Yes, so people don't get an "Allow?" screen |
| Scopes | `openid profile email` |
| Grant types | `authorization_code` (no `refresh_token`: LS3 revokes any refresh token anyway) |
| Token endpoint auth | `client_secret_basic` |
| Redirect URIs | `http://localhost:5180/callback` (local dev) and `https://s3.lovegood.in/callback` (live) |

No separate post-logout address is needed: `/login` on those origins is allowed because it
shares an origin with a redirect URI. Put the client ID and secret in `.env` as
`LS3_OIDC_CLIENT_ID` and `LS3_OIDC_CLIENT_SECRET`, never in a committed file. The local
dev copy (`./loony --env .env.dev`) reads them from there too.

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
