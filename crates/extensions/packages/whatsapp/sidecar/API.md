# WhatsApp sidecar HTTP contract

This is the contract between IronClaw and the WhatsApp sidecar. It has two
directions:

- **IronClaw → sidecar:** a small HTTP API (send a reply, block a contact,
  fetch media, manage the link, unlock the store).
- **Sidecar → IronClaw:** signed webhook events (customer messages, owner
  activity, link state, "I am locked").

Version: `v1`. Every route lives under `/v1/` except `/healthz`. A breaking
change gets a new prefix.

## Conventions

- Bodies are JSON (`Content-Type: application/json`), UTF-8. Request bodies
  over `SIDECAR_MAX_BODY_BYTES` (default 512 KiB) are rejected with `413`.
- Unknown request fields are rejected with `400` (`"field"` names the field).
- Times are RFC 3339 UTC strings with milliseconds, e.g.
  `"2025-10-09T08:53:20.000Z"`.
- "Characters" below means UTF-16 code units (JavaScript string length).
- Every error response has the shape:

  ```json
  { "error": "<code>", "message": "<optional short text>", "...": "optional extra fields" }
  ```

  `error` is a stable machine-readable code. Error bodies never contain stack
  traces, file paths, message text or phone numbers.
- Responses carry `Cache-Control: no-store` and
  `X-Content-Type-Options: nosniff`.

### Order of checks

For every `/v1/` route the sidecar checks, in order:

1. the route exists (`404 not_found`) and the method is right
   (`405 method_not_allowed`, with an `Allow` header);
2. the bearer token (`401 unauthorized`);
3. the lock state: every route except `POST /v1/unlock` answers
   `423 {"error":"locked"}` while the store is locked;
4. the body, in this order: `413 body_too_large`, `400 invalid_request` when a
   route needs a body and none was sent, `415 unsupported_media_type`,
   `400 invalid_json`, then `400 invalid_request` for the fields;
5. the operation itself.

### Authentication

Every `/v1/` route requires

```
Authorization: Bearer <SIDECAR_BEARER_TOKEN>
```

The token is compared in constant time (both sides are hashed with SHA-256
and compared with `crypto.timingSafeEqual`). A missing or wrong token gets
`401 {"error":"unauthorized"}` with `WWW-Authenticate: Bearer`. The token is
at least 32 characters and contains no whitespace.

## Identifiers

All identifiers are **opaque strings**. IronClaw stores and echoes them; it
must not parse them or build them.

| Field | Format | Bound |
|---|---|---|
| `chat_id` | WhatsApp JID of a 1:1 chat, normalised (no device suffix): `<digits>@s.whatsapp.net`, `<digits>@lid`, or the hosted forms `@hosted` / `@hosted.lid` | 128 chars |
| `contact_id` | WhatsApp JID of the customer, same forms | 128 chars |
| `message_id` | WhatsApp message id (e.g. `3EB0…`) | 128 chars |
| `media_id` | sidecar-issued, `med_` + 32 hex digits | 128 chars |
| `event_id` | sidecar-issued, `evt_` + hex digits; time-sortable | 64 chars |

### Phone-number JIDs and LIDs

WhatsApp addresses a person either by a **phone-number JID**
(`447700900123@s.whatsapp.net`, which contains the phone number) or by a
**LID** (`123456789012345@lid`, a per-account opaque id that hides the number).
The same customer can show up under either form, and the form can change over
time. Baileys sometimes reports both (`remoteJid` plus `remoteJidAlt`).

The sidecar keeps identifiers stable:

- **`chat_id`** is the first JID the sidecar saw for that customer. It never
  changes afterwards. Any other JID seen for the same customer is stored as an
  alias, so later messages under the other form map to the same `chat_id`.
- **`contact_id`** is fixed when the chat is first seen: the LID if one was
  known at that moment, otherwise the phone-number JID. It never changes
  afterwards.
- **`contact_phone_number`** (digits only, E.164 without `+`) is included once
  the sidecar has seen a phone-number JID for the contact. It is personal data.
- Device suffixes (`447700900123:12@s.whatsapp.net`) are removed.

A phone-number JID is personal data. IronClaw should treat `chat_id` and
`contact_id` like phone numbers when it logs or displays them.

## IronClaw → sidecar

### `POST /v1/unlock`

Unlocks the encrypted store with IronClaw's data key. Allowed while locked.

Request:

```json
{ "data_key": "<base64 of exactly 32 bytes>" }
```

`data_key` is standard base64 with padding (44 characters).

Responses:

| Status | Body | Meaning |
|---|---|---|
| `200` | `{"state":"unlocked","initialized":true}` | First unlock of an empty store: the key-check record was created with this key. |
| `200` | `{"state":"unlocked","initialized":false}` | The key matched the key-check record (or the store was already unlocked with this key). |
| `400` | `{"error":"invalid_request","field":"data_key",…}` | Not 32 bytes of base64. |
| `403` | `{"error":"wrong_key",…}` | The key does not match the key-check record (or differs from the key the store is already unlocked with). The store stays as it was. |
| `409` | `{"error":"key_check_missing",…}` | The volume holds records but no key-check record. The sidecar refuses to adopt a new key over data written under an unknown one. The operator must clear the volume and re-link. |
| `500` | `{"error":"store_corrupt"}` | The key-check record is unreadable. |

The call is idempotent, and concurrent calls are handled one at a time, so
they never start two WhatsApp connections. The key is checked in constant time
against an HMAC-SHA256 verifier; the key itself is never written to disk, and
the request bytes that carried it are zeroed once parsed (the parsed string
stays in memory until garbage collection). After unlocking, the sidecar loads
its event spool, starts delivering events and, if an account is linked,
connects to WhatsApp. If any of that fails, the store is locked again and the
call answers `500`.

### `POST /v1/send`

Sends a text reply to a chat.

Request:

```json
{
  "chat_id": "447700900123@s.whatsapp.net",
  "text": "Yes, 4pm on Tuesday works.",
  "quoted_message_id": "3EB0C7…",
  "idempotency_key": "run-7f3a:reply-1"
}
```

| Field | Rules |
|---|---|
| `chat_id` | required; a `chat_id` from a `message.received` event (or one of its aliases) |
| `text` | required; 1–65 536 characters; not only whitespace |
| `quoted_message_id` | optional; a `message_id` received in that chat |
| `idempotency_key` | required; 1–128 characters from `[A-Za-z0-9._:-]` |

Success, after WhatsApp's server acknowledged the message:

```json
{ "message_id": "3EB0A1B2C3D4E5F6A7B8C9", "server_ack_at": "2025-10-09T08:53:21.000Z" }
```

`server_ack_at` is the time from WhatsApp's server ack. It is evidence that
WhatsApp's server accepted the message, not that the customer's phone received
it.

Errors:

| Status | `error` | Meaning |
|---|---|---|
| `403` | `not_a_known_chat` | No message has arrived from this chat. The sidecar only sends to chats that have written to this number. |
| `403` | `contact_blocked` | The contact is blocked. |
| `409` | `idempotency_conflict` | The key was used before with a different `chat_id`, `text` or `quoted_message_id`. |
| `409` | `send_outcome_unknown` | An earlier attempt with this key did not finish (for example the sidecar restarted mid-send). Includes `message_id`. Treat the send as unverified; do not retry with a new key without checking. |
| `422` | `unknown_quoted_message` | `quoted_message_id` is not a message received in this chat (or it is older than the retention period). |
| `429` | `rate_limited` | Includes `retry_after_secs` (integer ≥ 1) and a `Retry-After` header. |
| `502` | `send_rejected` | WhatsApp answered with a negative ack. Includes `code` (WhatsApp's error code, e.g. `"463"`) and `message_id`. |
| `502` | `send_failed` | Handing the message to the socket failed, so WhatsApp never saw it. |
| `503` | `not_connected` | No live connection to WhatsApp (unlinked, or reconnecting), including a connection that dropped mid-send. Nothing was sent. |
| `504` | `ack_timeout` | The message was handed to WhatsApp but no server ack arrived within `SIDECAR_SEND_ACK_TIMEOUT_MS` (default 30 s). Includes `message_id`. Report the send as unverified. |

**Idempotency.** The result is stored under `idempotency_key` (for
`SIDECAR_MESSAGE_RETENTION_DAYS`). Repeating the same request with the same key
never sends twice. Requests with the same key are handled one at a time: one
that arrives while another is in progress waits for it, then gets its result.

- after a success it returns the same `200` body;
- after `ack_timeout` it returns `504 ack_timeout` again, or `200` once a late
  ack has arrived (also after a restart, if WhatsApp still sends the ack);
- after `send_rejected` or `send_failed` it returns the same `502`;
- after `not_connected` nothing is stored, so a retry with the same key sends.

**Rate limits.** Token buckets, one per chat and one global, both must have a
token. The defaults are 6 per minute per chat with a burst of 3, and 20 per
minute overall with a burst of 5 (see the README for the variables). Denied
requests consume nothing.

### `POST /v1/contacts/{contact_id}/block` and `/unblock`

Blocks or unblocks a contact on WhatsApp. `{contact_id}` is URL-encoded
(`@` may be sent as `%40`). The body is empty or `{}`.

| Status | Body |
|---|---|
| `200` | `{"contact_id":"…","blocked":true}` (or `false`) |
| `404` | `{"error":"unknown_contact"}`: the contact has never written to this number |
| `502` | `{"error":"block_failed"}`: WhatsApp refused, or the LID/phone-number mapping is not known yet |
| `503` | `{"error":"not_connected"}` |

The sidecar remembers blocked contacts. Messages from a blocked contact are
still forwarded (with `"blocked": true`), so IronClaw keeps a record, and
`/v1/send` to that chat is refused.

### `GET /v1/media/{media_id}`

Downloads and decrypts a media object referenced by a `message.received`
event, and streams its bytes.

- `200`: the body is the decrypted bytes.
  `Content-Type` is the media's MIME type (or `application/octet-stream` if it
  is malformed), with `Content-Disposition: attachment`. The body is chunked;
  there is no `Content-Length`. If the download fails after the headers were
  sent, or the stream grows past the cap, the connection is closed before the
  terminating chunk. **A body without the terminating chunk is a failed
  download**, not a short file.
- `404 unknown_media`: no such media id. Media ids are pruned together with
  their message after `SIDECAR_MESSAGE_RETENTION_DAYS`, so an expired media id
  also answers `404`.
- `410 media_expired`: the media id is known but its message reference is
  gone (only if pruning stopped part-way).
- `413 media_too_large`: the declared size exceeds `SIDECAR_MEDIA_MAX_BYTES`
  (default 64 MiB). Includes `max_bytes`.
- `502 media_download_failed`: WhatsApp's media servers failed (expired media
  is re-requested from the phone automatically when connected).

### `POST /v1/link/begin`

Starts linking a WhatsApp account to the sidecar as a linked device.

Request:

```json
{ "mode": "qr" }
```

or

```json
{ "mode": "pairing_code", "phone_number": "+447700900000" }
```

`phone_number` (required for `pairing_code`, ignored for `qr`) is the business
number being linked: 7–15 digits, with an optional leading `+`.

Starting a flow discards any unfinished flow and any partial credentials. The
call waits (up to 30 s) for the first display frame:

```json
{ "display_kind": "qr_code", "payload": "<opaque string>", "expires_in_secs": 60 }
```

```json
{ "display_kind": "pairing_code", "payload": "ABCD-EFGH", "expires_in_secs": 180 }
```

- `qr_code`: `payload` is an opaque string to render, unchanged, as a QR code. WhatsApp issues a
  new one after the first 60 s and then every 20 s. Poll `/v1/link/status`
  for the current one.
- `pairing_code`: `payload` is the 8-character code shown as `XXXX-XXXX`. The
  owner types it on the phone under *Linked devices → Link with phone number*.

Errors: `409 already_linked` (unlink first), `400 invalid_request`,
`502 pairing_code_failed`, `504 link_timeout` (no frame in 30 s; the flow
keeps running), `409 link_ended` (the flow ended before showing a frame;
`reason` says why).

A flow ends after `SIDECAR_LINK_FLOW_TTL_SECS` (default 300 s) if the phone
does not complete it.

### `GET /v1/link/status`

```json
{
  "state": "linked",
  "connected": true,
  "account_label": "Acme Plumbing",
  "linked_at": "2025-10-09T08:53:20.000Z"
}
```

| Field | Meaning |
|---|---|
| `state` | `unlinked`, `linking`, `linked` or `logged_out` |
| `connected` | the socket to WhatsApp is open right now |
| `account_label` | the account's push name, or `…` + the last four digits of its number; at most 256 characters |
| `linked_at` | when this account was linked (only when `linked`) |
| `display` | while `linking`: the current `{display_kind, payload, expires_in_secs}` |

`logged_out` means WhatsApp ended the session (the owner removed the device on
the phone, or the device was inactive for too long). The credentials and the
known-chat list are gone. Run `/v1/link/begin` again.

### `POST /v1/link/cancel`

Ends a `linking` flow and discards partial credentials. Returns the same body
as `/v1/link/status`. A no-op in other states.

### `POST /v1/unlink`

Logs the device out of WhatsApp (best effort), wipes the store and locks it.

- Deleted: Baileys credentials and signal keys, known chats and aliases,
  blocked contacts, message and media references, sent-message copies,
  idempotency results, link metadata.
- Kept: the key-check record and **undelivered webhook events** (delivered
  after the next unlock).
- The sidecar's copy of the data key is overwritten in memory; the sidecar is
  locked again and sends `sidecar.locked`. Unlock it with the same data key.

Response: `200 {"state":"unlinked","locked":true}`. No `link.state_changed`
event is sent for an unlink IronClaw asked for.

### `GET /healthz`

No authentication. Reveals only liveness and the lock state:

```json
{ "status": "ok", "locked": true }
```

Always `200` while the process serves requests. The Docker `HEALTHCHECK` uses
it.

## Sidecar → IronClaw: webhook events

The sidecar POSTs each event to `IRONCLAW_WEBHOOK_URL`
(for IronClaw: `/webhooks/extensions/whatsapp/events`).

```
POST <IRONCLAW_WEBHOOK_URL>
Content-Type: application/json
User-Agent: ironclaw-whatsapp-sidecar/0.1
X-Sidecar-Timestamp: 1760000000
X-Sidecar-Signature: v1=f9766a96ae85c8653d570df10c9d12d0529df31c148168168025c3f40c4fc384

{"event_id":"evt_…","type":"…","occurred_at":"…", …}
```

Redirects are not followed.

### Signing

```
signed_payload = "v1:" + <X-Sidecar-Timestamp value> + ":" + <raw body bytes>
signature      = lowercase_hex( HMAC-SHA256( key, signed_payload ) )
X-Sidecar-Signature: "v1=" + signature
```

- `key` is the UTF-8 bytes of `SIDECAR_WEBHOOK_HMAC_KEY`, as is (not decoded
  from hex or base64). It is at least 32 characters.
- `X-Sidecar-Timestamp` is the Unix time in whole seconds when the attempt was
  made. Every retry gets a fresh timestamp and signature; the body is
  byte-identical across retries.
- The receiver should reject timestamps more than 300 s from its clock.

This is exactly IronClaw's `hmac_sha256` ingress verification recipe:

```toml
[channel.ingress.verification]
kind = "hmac_sha256"
secret_handle = "<the handle holding SIDECAR_WEBHOOK_HMAC_KEY>"
signature_header = "X-Sidecar-Signature"
signature_prefix = "v1="
signature_encoding = "hex"
timestamp_header = "X-Sidecar-Timestamp"
max_age_seconds = 300
signed_payload = [
  { literal = "v1:" },
  { header = "X-Sidecar-Timestamp" },
  { literal = ":" },
  { body = true },
]
```

**Test vector** (also checked by `test/signer.test.ts`):

| | |
|---|---|
| key | `whatsapp-sidecar-test-vector-key-0123456789` |
| `X-Sidecar-Timestamp` | `1760000000` |
| body (112 bytes, no trailing newline) | `{"event_id":"evt_0199c8a1b2c3d4e5f60718293a4b","type":"sidecar.locked","occurred_at":"2025-10-09T08:53:20.000Z"}` |
| `X-Sidecar-Signature` | `v1=f9766a96ae85c8653d570df10c9d12d0529df31c148168168025c3f40c4fc384` |

### Delivery

- **Acknowledge with any `2xx`.** The response body is ignored.
- Anything else (another status, a timeout after `SIDECAR_WEBHOOK_TIMEOUT_MS`,
  default 10 s, or a network error) is retried with exponential backoff and
  jitter: base `SIDECAR_WEBHOOK_RETRY_BASE_MS` (1 s), doubling, capped at
  `SIDECAR_WEBHOOK_RETRY_MAX_MS` (5 min). Retries never stop.
- Every event except `sidecar.locked` is written to the encrypted spool
  **before** the first attempt and deleted only after a `2xx`. It survives
  restarts, lock cycles and unlink.
- Delivery is **in order and one at a time**: a failing event blocks the ones
  behind it. IronClaw should answer `2xx` for any event it has durably
  recorded, including ones it chooses to ignore.
- Delivery is **at least once**. `event_id` is unique and stays the same across
  retries; IronClaw must de-duplicate on it. A `message.received` whose event
  could not be spooled is forwarded when WhatsApp delivers the message again,
  under the same `event_id`.
- The gap: Baileys acknowledges a message to WhatsApp before the sidecar has
  spooled its event. If the sidecar dies in between, or cannot write to its
  volume, WhatsApp may not deliver that message again, and it is lost.

### Envelope

Every event is a JSON object:

| Field | Type | Notes |
|---|---|---|
| `event_id` | string | unique, stable across retries, ≤ 64 chars |
| `type` | string | one of the types below |
| `occurred_at` | string | RFC 3339, when the sidecar created the event |
| … | | the type's own fields, at the top level |

Unknown fields may be added in later `v1` revisions; receivers must ignore them.

### `message.received`

A customer wrote to the business number (1:1 chat, not from the owner).

```json
{
  "event_id": "evt_0199c8a1b2c34f1e2d3c4b5a6978",
  "type": "message.received",
  "occurred_at": "2025-10-09T08:53:21.120Z",
  "message_id": "3A1B2C3D4E5F60718293",
  "chat_id": "447700900123@s.whatsapp.net",
  "contact_id": "447700900123@s.whatsapp.net",
  "contact_phone_number": "447700900123",
  "contact_display_name": "Customer A",
  "content_kind": "image",
  "text": "Here is a photo of the leak",
  "media": [
    { "media_id": "med_0199c8a1b2c3a1b2c3d4e5f6a7b8", "mime_type": "image/jpeg", "size_bytes": 183204 }
  ],
  "quoted_message_id": "3EB0A1B2C3D4E5F6A7B8C9",
  "blocked": false,
  "received_at": "2025-10-09T08:53:20.000Z"
}
```

| Field | Type | Bound / notes |
|---|---|---|
| `message_id` | string | ≤ 128 chars |
| `chat_id` | string | ≤ 128 chars; the value to pass to `/v1/send` |
| `contact_id` | string | ≤ 128 chars; the value to pass to `/v1/contacts/…` |
| `contact_phone_number` | string, optional | digits, ≤ 20 |
| `contact_display_name` | string, optional | the sender's own WhatsApp push name (untrusted, chosen by the sender), ≤ 256 chars |
| `content_kind` | string | `text`, `image`, `video`, `audio`, `document`, `sticker`, `location`, `contact`, `other` |
| `text` | string, optional | the message text or media caption, ≤ 65 536 chars (WhatsApp's own limit) |
| `text_truncated` | `true`, optional | present only if `text` was cut to the bound |
| `media` | array | 0 or 1 items today, at most 10 |
| `media[].media_id` | string | for `GET /v1/media/{media_id}` |
| `media[].mime_type` | string | as declared by the sender, ≤ 127 chars |
| `media[].size_bytes` | integer or `null` | as declared by the sender |
| `media[].file_name` | string, optional | documents only, ≤ 255 chars, untrusted |
| `quoted_message_id` | string, optional | the message this one replies to, ≤ 128 chars |
| `blocked` | boolean | the contact is blocked in the sidecar |
| `received_at` | string | WhatsApp's message timestamp |

Every string here is untrusted customer input.

Not forwarded: groups (`@g.us`), status updates (`status@broadcast`),
broadcast lists (`@broadcast`), newsletters/channels (`@newsletter`), bots,
the owner's own "message yourself" chat, protocol messages (deletes, edits,
ephemeral settings), reactions, and decryption stubs. Only Baileys
`messages.upsert` batches of type `notify` are considered; history sync is
never forwarded. A message delivered twice by WhatsApp is forwarded once.

The first `message.received` from a chat makes that chat **known**, which is
what allows `/v1/send` to it.

### `owner.activity`

The owner sent a message in a known chat from the phone (or another linked
device). IronClaw uses this to mark the chat as handled by the owner.

```json
{
  "event_id": "evt_…",
  "type": "owner.activity",
  "occurred_at": "2025-10-09T09:01:02.000Z",
  "chat_id": "447700900123@s.whatsapp.net",
  "message_id": "3EB0FFEEDDCCBBAA9988",
  "sent_at": "2025-10-09T09:01:01.000Z"
}
```

The message text is not included. Owner messages in chats no customer started,
and messages this sidecar sent itself, are not reported.

### `link.state_changed`

```json
{ "event_id": "evt_…", "type": "link.state_changed", "occurred_at": "…", "state": "linked", "account_label": "Acme Plumbing" }
```

Sent when an account is linked for the first time (not on every reconnect)
and when WhatsApp logs the device out (`"state": "logged_out"`). `state` is
`linked` or `logged_out`; `account_label` is optional, ≤ 256 chars.

### `sidecar.locked`

```json
{ "event_id": "evt_…", "type": "sidecar.locked", "occurred_at": "…" }
```

Sent when the sidecar starts (it always starts locked) and after
`/v1/unlink`. IronClaw should respond by calling `POST /v1/unlock`. This event
cannot be spooled (the spool is encrypted), so it is retried from memory until
it gets a `2xx` or the sidecar is unlocked. It is never sent while unlocked.

## Notes for the IronClaw adapter

- On `423` from any route, unlock with the data key and retry once.
- Treat `504 ack_timeout` and `409 send_outcome_unknown` as "unverified", never
  as delivered. Only a `200` from `/v1/send` is delivery evidence.
- Fetch media while handling `message.received`; message references are kept
  for `SIDECAR_MESSAGE_RETENTION_DAYS` (default 30).
- De-duplicate events on `event_id`.
