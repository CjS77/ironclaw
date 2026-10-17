# ironclaw_intake — working rules

- Own the intake item record grammar and its invariants only. Channel
  routing, verification, and the read tools live in the extension host and
  host runtime.
- Items are never deleted. Do not add a delete or purge operation; the
  `/intake` mount deliberately grants no delete permission.
- Status moves forward only, and every transition is appended to
  `status_history`. Add a status only together with the code that produces
  it.
- The item id is derived from the event identity. Changing the derivation is
  a persisted-schema change: replays of already-stored events would then
  create second items.
- Every read and mutation is scoped by the owner. The mount is per user and
  is the isolation boundary; a record whose owner does not match the caller
  is a misconfigured mount, and the store fails closed with
  `IntakeError::ForeignRecord` (one check, `ensure_owned`, on every read
  path). Never filter such a row out silently.
- Persistence goes through `ScopedFilesystem` and `cas_update`. Requests read
  declared ordered projections; never list directories or scan all items.
- Stored text and attachment fields are outsider-controlled. Never log them
  and never hand them to a model without the untrusted-content envelope.
- No vendor names in this crate.

## Validation

- `cargo test -p ironclaw_intake`
- `cargo clippy -p ironclaw_intake --all-targets -- -D warnings`
- `cargo test -p ironclaw_architecture_tests`
