# ADR-002: Sessions, images and regeneration in the kernel

## Status

Accepted. Extends [ADR-001](ADR-001-assistant-kernel.md).

## Context

ADR-001 moved model calls, conversation memory and the tool loop into a
separate kernel, while the site and CRM kept ownership of their data. The first
release stored one conversation per user, had no session list, no images and no
way to retry an answer.

The CRM needed a real assistant workspace: many sessions per user, findable and
named, with the ability to attach a screenshot to a bug report and ask "what is
wrong here?". The CRM already owned file storage and upload policy, so the
question was where image bytes should live and who may read them.

## Decision

### Sessions belong to the kernel

The kernel owns the conversation lifecycle: title, pin, archive, delete and the
list of sessions for an actor. Title is derived from the first user message and
is not overwritten afterwards. `PATCH /v1/conversations/{id}` changes metadata;
`regenerate` re-runs the last user turn.

Each session endpoint requires the `X-Kernel-Channel` header and scopes reads
and writes to `(channel, actor_id)`. A session from another actor or channel is
`403`, not a silent empty list.

The CRM keeps `ai_conversations` as a mirror because `ai_actions` references it
and because a short kernel outage must not empty the sidebar. The mirror is
never authoritative: the kernel returns the session list and the title, the CRM
falls back to its mirror only when the kernel is unreachable.

### Images are referenced, not uploaded to the kernel

The CRM uploads to its own storage (existing `FileAttachment`), then sends
`attachments: [{id, name, mime}]` with the chat request. The kernel calls

```
GET {provider}/files/{file}?conversation_id=...&actor_id=...
```

with the provider service token. The provider checks that the actor owns the
file and that the file is attached to a message in that conversation, then
streams the bytes. The kernel validates the MIME and size, base64-encodes the
image for the current model call and stores only the reference in SQLite.

Why not base64 in the request body:

- the kernel would receive and store domain files, duplicating storage and
  retention policy;
- a 20 MB JSON body would need a much larger request limit for no benefit;
- access checks would move to the kernel, which has no user database.

Why not read CRM's disk:

- that is a shared secret and a shared failure domain;
- it violates database-per-service and file-ownership boundaries.

### Only the current turn carries pixels

Stored history replays a textual marker ("к сообщению приложено изображений: N")
instead of the image. Sending the image again on every turn would multiply cost
and token pressure without adding context. `regenerate` re-fetches the images of
the last user message, because the answer is being produced again.

### Vision is a route capability

`KERNEL_CLOUD_VISION` and `KERNEL_LOCAL_VISION` describe the endpoint, not the
kernel. A local llama.cpp server needs `--mmproj` to accept images. If an image
arrives on a text-only route the kernel returns `422 unprocessable` with a clear
message instead of dropping the attachment.

## Consequences

Positive:

- the sidebar, search, pin and archive work without a CRM read of kernel
  internals;
- images never leave the domain's storage and access policy;
- retry does not duplicate questions in the transcript;
- cost of a long image conversation stays bounded;
- a text-only local deployment fails loudly, not silently.

Negative:

- one extra provider round trip per attached image;
- the CRM mirror can lag the kernel; the UI prefers kernel data and falls back;
- the kernel now owns one more piece of user-visible state (titles), so a
  rename must succeed in two places or be retried.

Mitigations:

- the mirror sync is best-effort and logged, never blocking the answer;
- list and title come from the kernel; the mirror exists only for the degraded
  path;
- attachment fetch has a provider timeout and an explicit size ceiling.
