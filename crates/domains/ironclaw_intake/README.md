# ironclaw_intake

Durable intake items. An extension channel can declare intake mode
(`[channel.intake]` in its manifest). A verified inbound message on such a
channel never starts an agent turn: the host stores it here as an intake item,
for the account owner to read, and acknowledges the webhook. The model reads
intake items through the read-only `builtin.intake.list` and
`builtin.intake.read` tools.

- **Family / layer:** `domains` / `substrates`
- **Use this when:** storing, listing, reading, or moving the status of a
  message that arrived on an intake-mode channel.
- **Do not use this when:** recording a user-visible notification
  (`ironclaw_notifications`), binding an external conversation to a thread or
  deduplicating ordinary channel input (`ironclaw_conversations`), or keeping
  a conversation transcript (`ironclaw_threads`).

## Why a crate of its own

`crates/domains/AGENTS.md` admits a new domains crate only for a distinct
record grammar with independent consumers. Intake items qualify, and they do
not fit inside `ironclaw_notifications`:

- Notifications are metadata-only by design: that crate forbids message
  bodies. An intake item exists to hold a third party's message text, which
  is a different trust class (untrusted outsider content).
- The consumers are different: intake channels write items, the intake read
  tools hand them to the model inside the untrusted-content envelope, and
  later triage work will move them through further statuses. None of those
  touch the notification inbox.

## Ports

The store is split into two halves so each consumer holds only what it
needs. `IntakeRecorder` (`record`, `mark_chat_handled_by_owner`) is the write
half, held by the intake channel sink in the extension host. `IntakeReader`
(`list`, `get`) is the read half, held by the `builtin.intake.*` tools in the
host runtime, which therefore cannot write. The read half writes nothing at
all, not even an index declaration: it works over a mount with no write
permission (pinned by the contract suite). `IntakeStore` implements both;
composition builds one and hands each consumer its half.

## Records and layout

One JSON record per item at `/intake/items/<item-id>.json` in the owner's
per-user `/intake` mount. The item id is derived from
`(extension, installation, external event id)`, so:

- creating an item is one create-if-absent CAS on its own path;
- a replayed webhook for the same event finds the existing record and the
  store reports a duplicate, with no second item and no rewrite;
- there is no separate deduplication record that could drift from the item.

Listing is newest first with an opaque keyset cursor. It reads declared
ordered projections (received time + id, optionally led by status), never a
directory listing, per the filesystem contract's bounded-query rule. A third
projection (chat + status + received time + id) lets the owner-activity
sweep find one chat's open items without scanning. No projection carries the
owner: each owner's items sit under that owner's own per-user mount, so every
query already reads one owner's rows. Only the write paths (recording an item
and the owner-activity sweep) declare the projections, before an owner's
first item; a list for an owner whose projections were never declared, who
therefore has no items, reads as an empty page.

Owner isolation is that per-user mount. The store keeps one backstop check:
every record names its owner, and any read path (record, list, get, the
sweep) that meets a record naming a different owner fails with
`IntakeError::ForeignRecord` rather than hiding or showing it, because that
can only mean the mount is misconfigured.

A record that does not decode (damaged, or written by a newer schema) does
not break listing or the sweep: both skip it, log its id at debug (never its
content), and keep going. The skipped row still takes its place in the scan,
so a page can hold fewer items than asked for, even none, while
`next_cursor` moves past it; the sweep resumes each batch after the last row
it scanned, so such a row can never stall it. A direct `get` of the record
still reports the decode failure.

Items are never deleted, and the composition mount grants no delete
permission.

Status only moves forward (`received` → `handled_by_owner`) and
every change is appended to the item's `status_history`.

What an item keeps: the owning user, extension and installation ids, the
external event id, the chat and contact references (the contact's display
name is the sender display name, when the message carried one), the message
text bounded to 64 KiB (cut on a character boundary, flagged
`text_truncated`), up to 32 attachments' metadata (id, MIME type, file name,
size; the bytes are not kept), the receipt time, and the status history.

## Storage bounds

Each item is bounded (above), but the number of items is not. Anyone who can
message an intake channel can create items as fast as the ingress route's
rate limit allows (`IngressRateLimitConfig` in the extension host, by default
120 requests per 60 seconds per installation), and since items are never
deleted the store grows for as long as messages arrive. There is no
per-sender or total cap. The design relies on the vendor side to limit
floods: the messaging account's own rate limits and spam controls, and the
channel's own sidecar or bridge, which decides what it forwards.

## Trust

Message text, the chat and sender ids, the sender display name, and every
attachment field except the size (id, MIME type, file name) are controlled by
the outsider. This crate stores them as data. Anything that hands them to a
model must wrap them with `ironclaw_prompt_envelope::wrap_untrusted`. The
MIME type is additionally held to a bare RFC 6838 `type/subtype` token (no
parameters, at most 255 bytes); metadata that fails is dropped and the item is
flagged `attachments_truncated`.

## Test support

The `test-support` feature (dev-dependencies only) exposes
`test_support::per_user_intake_mount` and `test_support::in_memory_intake_store`,
the per-user `/intake` mount the store's consumers share in their tests.

## Validation

```bash
cargo test -p ironclaw_intake
cargo clippy -p ironclaw_intake --all-targets -- -D warnings
```
