# Todo: Sign in with Loony and projects

Nothing here is built yet. The sign-in design and its reasoning are in
[loony-auth-signin.md](loony-auth-signin.md).

## The result

1. A user logs in to the web UI with their Loony account.
2. They see the projects they created and the projects they've been invited to.
3. Inside a project, they see its buckets, access keys and members.
4. Their apps connect with one of the project's access keys, using a standard S3 SDK.

## Decisions made

- **Accounts:** people use their normal Loony account, in loony-auth's `loony`
  organization. LS3 keeps no passwords.
- **Projects replace accounts for storage:** a project gets its own `OwnerId`, so
  buckets and keys belong to the project. LS3's existing ownership checks then keep
  projects apart.
- **Roles:** Owner can do everything. Member can manage buckets and access keys, and see
  who's in the project. A read-only role comes later.
- **Creating projects:** only people an admin approves, until storage limits are enforced.
- **Invites:** per project, by email, and shown inside the UI. LS3 won't send email in the
  first version.
- **Temporary keys:** the browser gets short-lived keys lasting about 8 hours, with no
  refresh tokens.
- **Root key and admin panel:** both stay, and the admin panel stays local-only.

## Phase 0: Fix the SDK bugs

Do this first, because every project's apps depend on it.

- [x] Decode `aws-chunked` upload bodies on `PutObject` and `UploadPart`, so the encoding
      is never stored as file content.
- [x] Support the streaming payload modes SDKs send, including
      `STREAMING-UNSIGNED-PAYLOAD-TRAILER` and `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`.
- [x] Verify the checksum sent in the trailer, and reject the upload if it doesn't match.
- [x] Reject any payload mode LS3 doesn't understand, rather than storing the raw bytes.
- [x] Accept bucket paths with a trailing slash (`/bucket/`), which fixes `CreateBucket`
      from the JavaScript SDK.
- [x] Verify plain bodies against a declared SHA-256 or `x-amz-checksum-*` header.
- [x] Add regression tests that upload and compare the returned bytes, for small files and
      multipart.
- [x] Rerun over HTTPS with default settings: boto3, the JavaScript SDK and the AWS CLI,
      covering create, upload, multipart, download, list, delete and presigned URLs.
- [ ] Rerun the same checks with rclone.
- [ ] Test signed chunks and signed trailers with a real client. No available client sends
      them; signed chunks match AWS's published example, and signed trailers only have
      tests written from the documented format.
- [x] Check the live server for objects uploaded before the fix. It had none.
- [x] Document the remaining SDK setup, such as path-style addressing and `s3v4` presigned
      URLs in boto3.
- [x] Remove the outdated "There's no API to create additional credentials yet" line from
      `docs/api-reference.md`.
- [x] Deploy to the live server.
- [ ] Add a Cloudflare cache rule bypassing the cache on `s3.lovegood.in`. Cloudflare turns
      `HEAD` into `GET` for file types it caches, which breaks the signature, and it keeps
      serving presigned links for those types after they expire.
- [x] Send `Cache-Control: private` on object responses, so an expired link stays dead even
      without that rule. It still lets a person's own browser cache what it fetched.
- [ ] Rerun the public test after both, including large uploads and parallel downloads.
- [x] Support `Range` requests, because `aws s3 cp` and boto3's `download_file` downloaded
      large files corrupted by default. Also handle `If-Match` and `If-Range`.

## Phase 1: Sign in with Loony (server)

**Credentials**

- [x] Add an optional expiry to `Credential`, with `#[serde(default)]` so existing records
      still load.
- [x] Reject expired keys in both header and presigned verification.
- [x] Add a periodic cleanup of expired keys.
- [x] Record what each key is for and who it was issued to. Only standard and user session
      keys exist so far: Phase 2 adds the project ones.

**Users**

- [x] Add a user record: `iss` and `sub`, email, email verified, name, status, whether they
      may create projects, and timestamps.
- [x] Add the Raft commands, snapshot support and tests. Generate IDs and timestamps before
      proposing a command, as `ResolvedCreateBucket` does, so every node stores the same
      values.

**Sign-in endpoint**

- [x] Add settings for the issuer URL, client ID, client secret and redirect URIs, with the
      secret kept in `scripts/.env`.
- [x] Add `POST /_console/v1/session`, which takes the code, PKCE verifier, nonce and
      redirect URI.
- [x] Only accept redirect URIs from the configured list.
- [x] Exchange the code at loony-auth's `/oauth/token`.
- [x] Verify the ID token against the JWKS: EdDSA only, plus issuer, audience, expiry and
      nonce.
- [x] Cache the JWKS, and refetch it when an unknown `kid` appears.
- [x] Create or update the user, refusing disabled users.
- [x] Return a user session key, which can call `/_console/v1/*` but owns no buckets.
- [x] Add rate limiting, because the endpoint is public.
- [x] Add sign-out, which revokes the user's session key.
- [x] Authenticate all console endpoints with SigV4 and the existing verifier.
- [x] Update CORS so only the configured UI origins can call `/_console/v1/*`.

**Tests**

- [x] Run against a fake identity provider, covering a good sign-in and a bad signature,
      wrong issuer, wrong audience, expired token, wrong nonce and reused code.
- [ ] Sign in against the real loony-auth, which needs a registered client (Phase 5).

## Phase 2: Projects (server)

**Projects**

- [x] Add a project record: ID, name, its `OwnerId`, status, creator and timestamps.
- [x] Add a membership record: project, user, role and timestamps.
- [x] Add the Raft commands, snapshot support and tests for both.
- [x] `GET /_console/v1/projects`: projects I created, projects I've joined, and my
      pending invites.
- [x] `POST /_console/v1/projects`: only for approved users, with a limit of 10 projects
      per person.
- [x] `GET` and `PATCH /_console/v1/projects/{id}`: view and rename.
- [x] `DELETE /_console/v1/projects/{id}`: Owner only, refused while it still has buckets,
      and also removes its keys, members and invites.

**Permissions**

- [x] Check membership and role on every project endpoint.
- [x] Return 404, not 403, for projects the user isn't in.
- [x] Add a table-driven test of each role against each endpoint.

**Browser access to a project**

- [x] `POST /_console/v1/projects/{id}/session`: returns a temporary key tied to that
      project and user, which the UI uses for buckets and files.
- [x] Revoke those keys immediately when the user is removed or disabled, or the project
      is disabled or deleted.

**Access keys**

- [x] List a project's keys, without secrets.
- [x] Create a key with a name, showing the secret once. A project can have 100.
- [x] Enable, disable and delete keys.
- [x] Record who created each key and when it was last used, updating last-used no more
      than about once an hour to limit Raft writes.

**Members and invites**

- [x] Add an invite record: project, email, role, who invited, expiry and status. An invite
      is deleted once answered, so only pending ones are stored.
- [x] Owner can invite by email, list and cancel invites, change roles and remove members.
- [x] Any member can leave.
- [x] The last Owner can't leave or be demoted.
- [x] Accept or decline an invite only if the signed-in email matches and is verified.
- [x] Expire invites after 7 days, and refuse duplicates.

**Audit**

- [x] Log every project, member, invite and access key change, with the user who made it.
- [x] Add console actions to the existing audit log, and never log secrets.

**Existing data**

- [x] Add an admin action to move an existing owner's buckets and keys into a project.
- [x] Leave the root key's own buckets as they are.

## Phase 3: Web UI

**Sign-in**

- [x] Add a **Sign in with Loony** button and a **Create account** button, which uses
      `prompt=create`.
- [x] Add a callback page that checks `state` and calls the sign-in endpoint.
- [x] Keep keys for the browser session only, not in `localStorage`. The root key's sign-in
      still is in `localStorage`, as it was.
- [x] When a key nears expiry, renew it through the redirect, without interrupting a
      running upload.
- [x] Renew before starting a big upload.
- [x] Sign out through loony-auth's `/oauth/logout`.
- [x] Show clear messages for failed sign-ins and disabled accounts.
- [x] Keep access key sign-in behind a small link for the root key and admin panel.

**Projects page**

- [x] Add **My projects** and **Shared with me** sections.
- [x] Show pending invites, with accept and decline.
- [x] Add a **New project** button, and explain how to get approved if the user can't
      create one.
- [x] Add empty states for new users.

**Inside a project**

- [x] Add a project layout with **Buckets**, **Access keys**, **Members** and **Settings**
      tabs, and a project switcher.
- [x] Reuse the existing bucket and file pages, using the project's temporary key.
- [x] Rework routes to `/projects/:id/buckets/:name`.
- [x] **Access keys:** create, disable and delete keys, show the secret once, and show
      copyable connection details with examples for Python, JavaScript and rclone.
- [x] **Members:** list members and invites, invite, change roles and remove. Hide what
      Members aren't allowed to do.
- [x] **Settings:** rename, leave and delete, with a confirmation that asks for the
      project's name.
- [x] Warn when a share link will stop working because the temporary key expires.
- [x] Test the full flow in a headless browser, including error states and phone widths.
- [x] Check the disabled-project screens in a browser.

## Phase 4: Admin panel

- [x] Admin API for users: list them, approve or remove project creation, and disable or
      enable them.
- [x] **Users page:** the same in the admin panel.
- [x] **Disable user:** ends their temporary keys immediately and refuses new ones. Project
      access keys keep working, because they belong to the project.
- [x] **Projects page:** list projects with their members and usage, and disable or enable
      a project.
- [x] **Disable project:** stops all of its keys immediately.
- [x] Add the move-existing-owner action from Phase 2.
- [x] Update `docs/admin.md`.

## Phase 5: Go live

These steps change live services, so confirm each one first.

- [ ] Back up the live data directory.
- [ ] Register LS3 as a confidential client in loony-auth's `loony` organization, with
      `openid`, `profile` and `email` scopes and exact redirect URIs.
- [ ] Add the client secret to `scripts/.env`.
- [x] Let `loony-server` serve the built UI on its own address, as this machine has no other
      web server, and test the production build in a browser.
- [ ] Host the UI at `s3-console.lovegood.in`, and add the tunnel route and DNS record.
- [ ] Deploy the new server and confirm the admin API is still local-only.
- [ ] Run the full flow on the live site with two real accounts: sign up, create a project,
      invite, accept, create a bucket and key, upload from an SDK, remove a member, and
      confirm they lost access.
- [ ] Confirm one project's key can't reach another project's buckets.
- [ ] Update `README.md`, `docs/usage.md` and `docs/configuration.md`.

## Phase 6: Storage limits, then open project creation

- [ ] Track each project's stored bytes and object count.
- [ ] Add a default project limit, with per-project overrides in the admin panel.
- [ ] Enforce the limit on `PutObject`, `UploadPart` and `CompleteMultipartUpload`.
- [ ] Show usage in the UI and the admin panel.
- [ ] Test the limit under concurrent uploads.
- [ ] Add a setting that lets every verified user create projects, then turn it on.

## Related work

**loony-auth**

- [ ] Test whether a disabled user can still use their refresh tokens and sessions. If so,
      fix it in loony-auth, because it also affects loony-library.

**Later, not needed for this work**

- [ ] Read-only role and keys limited to one bucket.
- [ ] Sending invites by email.
- [ ] `DeleteObjects`, for bulk delete from SDKs.
- [ ] Share links that outlast a temporary key.
- [ ] Handling SIGTERM so `systemctl stop` drains requests.
