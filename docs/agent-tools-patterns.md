# Agent Tools Pattern Library Requirements

## Purpose

`agent-tools` should expose the gateway global pattern library as a first-class
CLI surface. Patterns are organization-wide markdown documents that describe how
we do things. They are not project-local tasks, and they are not memory entries.

## Gateway API

All endpoints require the same bearer token used by the existing gateway API.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/v1/patterns?q=<query>&label=<label>&version=<version>&state=<state>&superseded_by=<id-or-slug>` | List or search pattern summaries. Search covers title, slug, summary, body, labels, version, state, and `superseded_by`. Filters are exact-match and can be combined with `q`. |
| `POST` | `/v1/patterns` | Create a pattern. |
| `GET` | `/v1/patterns/:id` | Fetch one pattern by id or slug, without comments. |
| `PATCH` | `/v1/patterns/:id` | Update pattern metadata or markdown body. |
| `DELETE` | `/v1/patterns/:id` | Delete a pattern. |
| `GET` | `/v1/patterns/:id/comments` | Fetch comments for one pattern. |
| `POST` | `/v1/patterns/:id/comments` | Add a comment to one pattern. |

Pattern create body:

```json
{
  "title": "Deploying Eventic Applications",
  "slug": "deploying-eventic-applications",
  "summary": "How we use main and tag deploys for independent sites.",
  "body": "# Deploying Eventic Applications\n\n...",
  "labels": ["eventic", "deploy"],
  "version": "draft",
  "state": "active",
  "superseded_by": null,
  "author": "agent-id"
}
```

Pattern response shape:

```json
{
  "id": "uuid-v7",
  "title": "Deploying Eventic Applications",
  "slug": "deploying-eventic-applications",
  "summary": "How we use main and tag deploys for independent sites.",
  "body": "# Deploying Eventic Applications\n\n...",
  "labels": ["eventic", "deploy"],
  "version": "draft",
  "state": "active",
  "superseded_by": null,
  "author": "agent-id",
  "created_at": 1777130000000,
  "updated_at": 1777130000000
}
```

List/search response shape is an array of summaries. Summaries omit `body` and
include `comment_count`.

`version` is lifecycle metadata, not semantic versioning. Allowed values are:

- `draft`: proposed or still being worked through.
- `latest`: current recommended practice.
- `superseded`: retained for historical discovery but not recommended.

`state` is required lifecycle metadata. Allowed values are `active` and
`archived`. For superseded patterns, set `version` to `superseded` and
`superseded_by` to the replacement pattern id or slug so agents can follow it.

`labels` are topical tags used for search and filtering, such as `linux`,
`systemd`, `services`, `eventic`, `deploy`, or `encryption`.

Structured list filters:

- `q`: broad text search across title, slug, summary, body, labels, version,
  state, and `superseded_by`.
- `label`: exact topical tag match, for example `label=systemd`.
- `version`: exact lifecycle match; must be `draft`, `latest`, or
  `superseded`.
- `state`: exact state match; must be `active` or `archived`.
- `superseded_by`: exact replacement pointer match by id or slug.

Comments are intentionally not included in `GET /v1/patterns/:id`. Agents should
only fetch comments when the user explicitly asks to address or review comments.

## CLI Surface

Recommended commands:

```bash
agent-tools patterns list
agent-tools patterns search "<query>" [--label x] [--version latest] [--state active] [--superseded-by slug]
agent-tools patterns get <id-or-slug>
agent-tools patterns create --title "..." --version draft --state active [--superseded-by id-or-slug when superseded] [--slug "..."] [--label x] [--summary "..."] --body-file path.md
agent-tools patterns update <id-or-slug> [--title "..."] [--version latest] [--state active|archived] [--superseded-by id-or-slug] [--slug "..."] [--label x] [--summary "..."] [--body-file path.md]
agent-tools patterns delete <id-or-slug>
agent-tools patterns comments <id-or-slug>
agent-tools patterns comment <id-or-slug> "<markdown comment>"
```

`get` must print only the pattern document and metadata. It must not fetch or
display comments.

This separation is important because comments are collaboration state, not
approved guidance. A pattern can have unresolved review notes, proposed edits,
or user discussion that should not be mixed into the normal context an agent
uses to perform work. Pulling comments by default would make agents more likely
to treat pending debate as current practice, increase token usage on every
lookup, and make old comment threads unexpectedly affect unrelated tasks.
Comments are opt-in so an agent only loads them when the user is explicitly
asking to review or resolve that discussion.

`comments` should call `GET /v1/patterns/:id/comments` and print the thread.

`comment` should call `POST /v1/patterns/:id/comments` with:

```json
{
  "content": "...",
  "author": "<agent id>",
  "author_type": "agent"
}
```

## Agent Behavior

Agents should use patterns as durable global guidance. They should search the
pattern library when the task appears to involve an established organizational
practice, such as deployment, encryption, secrets handling, project setup,
frontend conventions, release workflows, or incident response.

When multiple matching patterns exist, agents should prefer `version=latest`.
If an otherwise relevant pattern has `version=superseded`, agents should inspect
`superseded_by` and fetch that replacement.
Draft patterns can inform discussion, but should not override latest patterns
unless the user explicitly asks to work on draft guidance.

Agents should not treat pattern comments as part of the normal guidance pull.
Comments are review/collaboration material and should be fetched only when the
user says comments exist or asks to address them.
