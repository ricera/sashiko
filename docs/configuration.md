# Configuration Reference

Sashiko is configured through two files in the project root:

- **Settings.toml** -- application settings (AI, server, git, review)
- **email_policy.toml** -- email delivery policy

Both can be bootstrapped from the examples in [docs/examples/](examples/).
All settings can also be overridden via environment variables using the
`SASHIKO` prefix with `__` (double underscore) as the separator (e.g.
`SASHIKO__AI__PROVIDER=gemini`).

For LLM provider-specific setup (API keys, auth, provider features), see
the [LLM Provider Configuration Guide](llm-providers.md).

## Settings.toml sections

### Top-level keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `log_level` | string | `"info"` | Default log filter for the daemon (e.g. `"warn"`, `"info"`, `"debug"`). `RUST_LOG` overrides it when set, `--debug` forces `"info"`, and `sashiko review` defaults to `"warn"` regardless of this value. |

### `[project]`

Optional. Describes the project this configuration is for.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `kind` | string | -- | Project this file is for: `"linux"` or `"sashiko"`. When set, it is checked against the project selected by `--project` / `SASHIKO_PROJECT`, and a mismatch is an error. When absent, the file is accepted for any project. |
| `name` | string | `""` | Display name shown in the web UI. |
| `description` | string | `""` | Short description shown in the web UI. |
| `domain` | string | `""` | Public hostname used to build links to the web UI in forge comments (`https://<domain>/#/patchset/...`). Falls back to `sashiko.sashiko.dev` when empty. |
| `attribution` | string | -- | Actor name recorded on bug discoveries. Defaults to `domain`, or `"sashiko"` when neither is set. |

### `[forge]`

Optional. Controls forge (GitHub/GitLab) webhook integration.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable forge webhook endpoint. |
| `disable_nntp` | bool | `true` | Disable NNTP ingestion when forge is enabled. |
| `provider` | string | -- | Forge provider: `"github"` or `"gitlab"`. |
| `webhook_secret` | string | -- | Webhook signing token or shared secret for authenticating incoming requests. When configured, requests are authenticated via signature verification, which is the only way a forge can authenticate. See the [Webhook Security Guide](WEBHOOK_SECURITY.md). |
| `api_token` | string | -- | Forge API token (for future API-based features). |

> **Security:** When `Settings.toml` contains secrets, restrict file
> permissions: `chmod 600 Settings.toml`.

### `[database]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | `"sashiko.db"` | Path to the SQLite database file. |
| `token` | string | `""` | Database token (unused for SQLite). |

### `[mailing_lists]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `track` | string or list | -- | Mailing lists to monitor. Accepts a TOML array or a comma-separated string. |

### `[nntp]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `server` | string | `"nntp.lore.kernel.org"` | NNTP server hostname. |
| `port` | integer | `119` | NNTP server port. |
| `tls` | bool | `false` | Wrap the session in TLS (implicit NNTPS). |

Setting `tls` does not change `port`; set it to `563` as well when
enabling NNTPS. The certificate is verified against the host trust
store, so an internal CA must be installed there. `lore.kernel.org`
offers no TLS-protected NNTP, so this is for internal mirrors.

### `[smtp]`

Optional. If omitted, no review emails are sent. Even when present,
`dry_run` defaults to `true` as a safety measure.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `server` | string | -- | SMTP server hostname. |
| `port` | integer | -- | SMTP server port. |
| `username` | string | -- | SMTP username (optional). |
| `password` | string | -- | SMTP password (optional). |
| `sender_address` | string | -- | From address for review emails. |
| `reply_to` | string | -- | Reply-To address (optional). |
| `dry_run` | bool | `true` | When true, emails are logged but not sent. |

### `[ai]`

Core AI settings that apply to all providers.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `provider` | string | -- | LLM provider: `gemini`, `claude`, `claude-cli`, `codex-cli`, `copilot-cli`, `bedrock`, `vertex`, `kiro-cli`, `goose`, `openai`, `openai-compatible`. |
| `model` | string | -- | Model identifier (provider-specific). |
| `max_input_tokens` | integer | `150000` | Maximum input tokens per request. |
| `max_interactions` | integer | `100` | Maximum tool-call rounds per review turn. |
| `temperature` | float | `1.0` | Sampling temperature. |
| `api_timeout_secs` | integer | `300` | Timeout for individual API calls (seconds). |
| `log_turns` | bool | `false` | Log each AI request/response turn at info level. Verbose but useful for debugging. |
| `response_cache` | bool | `false` | Cache AI responses to disk. The daemon keeps the cache beside its database; a local review, which has none, keeps it under `$XDG_DATA_HOME/sashiko/`. Entries are keyed on the provider's own settings as well as the request, so changing `model`, an endpoint, a reasoning level, or an output cap misses the entries recorded under the old value rather than replaying them. |
| `response_cache_ttl_days` | integer | `7` | TTL for cached responses (days). Entries stranded by a settings change age out on this schedule. |

#### `[ai.claude]`

Settings specific to the Claude API provider (`provider = "claude"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `prompt_caching` | bool | `true` | Enable Anthropic prompt caching (5-minute TTL). |
| `max_tokens` | integer | `4096` | Max output tokens per response. |
| `base_url` | string | -- | Override the API base URL (optional, for proxies like Portkey). |
| `thinking` | string | -- | Extended thinking mode: `"enabled"` or `"adaptive"` (Sonnet 4.6+). |
| `effort` | string | -- | Thinking effort: `"low"`, `"medium"`, `"high"`. |

#### `[ai.claude_cli]`

Settings for the Claude Code CLI provider (`provider = "claude-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `effort` | string | -- | Thinking effort: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. |

#### `[ai.codex_cli]`

Settings for the Codex CLI provider (`provider = "codex-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `effort` | string | -- | Reasoning effort: `"none"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Passed as `-c model_reasoning_effort=<effort>`, which outranks `~/.codex/config.toml` but not an enterprise-managed requirements layer. A run whose effort that layer substitutes fails. |

#### `[ai.gemini]`

Settings for the Gemini provider (`provider = "gemini"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `explicit_prompt_caching` | bool | `false` | Use explicit caching hints in requests. |

#### `[ai.openai_compat]`

Settings for the OpenAI providers (`provider = "openai"` or `provider = "openai-compatible"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `base_url` | string | model-derived | API endpoint URL. Derived from the model name, `https://api.openai.com/v1/chat/completions` for anything unrecognized. |
| `context_window_size` | integer | model-derived | Context window size. `128000` for most models. |
| `max_tokens` | integer | `4096` | Max output tokens per response. With `provider = "openai"` it is sent as `max_completion_tokens`, which bounds reasoning tokens as well as the reply. |

#### `[ai.kiro_cli]`

Settings for the Kiro CLI provider (`provider = "kiro-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `binary` | string | `"kiro-cli"` | Path to the kiro-cli binary. |
| `agent` | string | -- | Custom agent name (optional). |
| `context_window_size` | integer | `200000` | Context window size. |

#### `[ai.goose_cli]`

Settings for the goose provider (`provider = "goose"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `binary` | string | `"goose"` | Path to the goose binary. |
| `goose_provider` | string | `"openai"` | Backend goose itself talks to, passed as GOOSE_PROVIDER. |
| `env` | table | `{}` | Environment for the goose child process, e.g. `OPENAI_HOST`. goose inherits Sashiko's environment and these entries win over it, but they cannot override the variables Sashiko pins to keep goose a completion backend (`GOOSE_MODE`, `GOOSE_MODEL`, `GOOSE_PROVIDER`, `GOOSE_CONTEXT_LIMIT`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`). |
| `context_window_size` | integer | `128000` | Context window size. goose adds roughly 5k tokens of its own prompt, so keep `max_input_tokens` well below this. |

### `[server]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `host` | string | `"::"` | Listen address. `"::"` binds to all interfaces (IPv4 and IPv6). |
| `port` | integer | `8080` | Listen port for the web UI and API. |
| `read_only` | bool | `false` | When true, disables write API endpoints. Set automatically by `--no-api`. |
| `public_base_url` | string | -- | The URL the service is reachable at from outside, with no trailing slash. Required whenever `[smtp]` is configured: sign-in links are mailed, and the bind address names no host a recipient can open. The server refuses to start without it. |
| `jwt_secret` | string | -- | Signs sign-in links and session tokens. Without it, sign-in returns `501` and no identity can be established. Keep it stable: replacing it invalidates every session and every unopened link. Prefer `SASHIKO__SERVER__JWT_SECRET` over writing it to disk. |
| `log_sign_in_links` | bool | `false` | Writes sign-in links to the log. For a developer machine with no real users; a link in a log is a credential anyone reading the log can spend. |

### `[server.acl]`

Capability lists, matched against the address a caller signed in with. Every
list is empty by default, which grants nothing (fail-closed). The local
operator token covers `ingest`, `cancel` and `review` for tooling that can read
the server's token file, so these lists are only about remote, identified
callers.

| Key | Type | Description |
|-----|------|-------------|
| `admins` | list | All capabilities, including root-level maintenance operations. |
| `security` | list | Reads and comments on every bug and reads raw AI transcripts. Grants no ingest, cancel or review. |
| `bug_reporters` | list | May file new bugs over HTTP. Empty means only admins can. |
| `ingest` | list | May submit patches (`/api/submit`). |
| `cancel` | list | May halt running workloads. |
| `review` | list | May trigger AI analyses of existing patches. |
| `blocklist` | list | Denies everything, overriding every grant above. |

Each list accepts either a TOML array or a comma separated string, so a
deployment can name its operators through the environment instead of baking
them into the image:

```bash
SASHIKO__SERVER__ACL__ADMINS="first@example.org,second@example.org"
```

### `[git]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `repository_path` | string | -- | Path to the kernel git repository used for patch application and context. |
| `references` | array of tables | empty | Optional read-only repositories consulted for grounding. Linux project only. See [`[[git.references]]`](#gitreferences). |

#### `[[git.references]]`

Optional ordered list of read-only repositories the review can read alongside
the code under review. Each entry is one table:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | -- | Required. What the model passes as `repo` to read this tree, and what errors and logs call it. Must match `^[a-z0-9][a-z0-9._-]{0,31}$` and be unique. `review` is reserved for the repository under review, and `kernel` may only name a `kind = "kernel"` entry. |
| `kind` | string | `"source"` | `"kernel"` for the Linux tree the reviewed code builds against, or `"source"` for any other tree. At most one entry may be `"kernel"`. |
| `path` | string | -- | Required. The top of a git work tree, or the git directory of a bare clone. Use an absolute path: a relative one is resolved against the working directory of whichever process reads it. |
| `revision` | string | `HEAD` | Revision to read the tree at: a tag, SHA or branch. Must not start with `-`. |
| `description` | string | -- | What the tree is and what to use it for, shown to the model. Required for `kind = "source"`; optional for `kind = "kernel"`, which has built-in text. |
| `applies_to` | list of strings | absent (every patch) | Globs, relative to the root of the repository under review, naming the files a patch must touch for this reference to be attached to its review. See [Scoping a reference](#scoping-a-reference-to-part-of-the-tree). |

At most 8 entries are allowed. Each adds a `repo` value to every git tool and an
entry to the prompt of every review it is attached to, so configure only trees
the reviews need.

#### Reviewing out-of-tree modules

When `repository_path` is an out-of-tree kernel module rather than a kernel tree,
the review has no `include/linux/` to read. The model then answers questions
about kernel APIs from training recall -- unversioned, possibly wrong for the
kernel you build against, and with no signal that it is doing so. The same holds
for any other tree the module talks to: the RDMA half of driver-a has an ABI
with the userspace provider in rdma-core, and that code is in neither the module
nor the kernel.

Add a reference for each such tree. For driver-a, the kernel it builds
against and the example-org/rdma-core release branch it ships with:

```toml
[git]
repository_path = "/srv/sashiko/driver-a"

[[git.references]]
name = "kernel"
kind = "kernel"
path = "/srv/sashiko/linux"
revision = "v6.18"
# No applies_to: the kernel is attached to every patch. See the kernel rule
# under "Scoping a reference to part of the tree".

[[git.references]]
name = "rdma-core"
path = "/srv/sashiko/rdma-core"      # a clone of example-org/rdma-core
revision = "origin/ionic-rdma"       # a release branch; see the notes on pins
applies_to = ["rdma/drv/**", "eth/ionic/ionic_aux*", "include/uapi/rdma/**"]
description = """Userspace RDMA stack (example-org/rdma-core). \
kernel-headers/rdma/ionic-abi.h mirrors the kernel uapi; providers/ionic is \
libionic, whose ionic_fw.h hand-mirrors the driver's WQE/CQE/doorbell formats. \
Use it to check the kernel<->provider ABI, ctx_resp capability/version fields \
and mmap offsets; old and new rdma-core must both keep working."""
```

`[[git.references]]` tables must come after every plain `[git]` key. In TOML, a
key written below a table header belongs to that table, so a `repository_path`
placed after the first `[[git.references]]` becomes an unknown key of that entry
and the settings fail to load. References are configured in `Settings.toml`
only: an array of tables cannot be expressed through `SASHIKO__` environment
variables.

This does several things:

- The git tools gain a `repo` argument whose values are `"review"` (the
  default, the code under review) followed by each reference's name, in
  configured order. Its description names each reference's kind and pin.
- The system prompt gains a "Read-only Reference Repositories" block listing
  each reference with its pin, the commit it resolved to and its description,
  and asking the model to cite `repo:file:line` for any contract it claims.
- When a `kind = "kernel"` reference is attached, the prompt also states that
  the repository under review is an out-of-tree module and withdraws the
  in-tree conventions that do not apply -- `Fixes:` tags, lore threads, and
  treating compat `#ifdef`s as needless complexity. A configuration with only
  `"source"` references, such as rdma-core alone, does not get this block.
- Every tool result read from a reference carries `repo_read`, the reference's
  name, and `revision_read`, the revision actually served, with `HEAD` reported
  as the commit SHA it was served.
- The list is validated before any review runs, at daemon startup and again
  each time a review worker starts, because a reference that does not resolve
  otherwise surfaces as tool errors the model works around, leaving a review
  that looks normal and is ungrounded. Errors name the entry, e.g.
  `git.references[rdma-core].revision 'v59' does not resolve in ...`.
- Each worker logs one line per reference when it starts, at INFO: its name,
  kind, path, configured revision with the commit it resolved to, and which
  patches it applies to, e.g.
  `Reference repository rdma-core: kind source, path /srv/sashiko/rdma-core,
  revision origin/ionic-rdma -> 9f8e7d6c..., applies to rdma/drv/**,
  eth/ionic/ionic_aux*, include/uapi/rdma/**` (or `applies to every patch`).

Notes:

- **Sashiko never fetches, writes to or checks out a reference repository.**
  Clone each one and keep it updated yourself, for example with a `git fetch`
  from cron.
- **Pins.** Each worker resolves every reference's revision to a commit when it
  starts and serves that commit for the whole review, so a fetch mid-review does
  not move it. A tag or SHA gives a fixed pin. A branch pin -- such as a
  example-org/rdma-core release branch -- moves as you update the clone: the next
  worker reads the new tip, and its INFO line records which commit that was. A
  revision left at `HEAD` behaves the same way for whatever the clone has
  checked out.
- **HEAD means the pin.** In a reference read, `HEAD`, git's `@` shorthand, an
  omitted revision, and forms such as `HEAD:include/linux/netdevice.h` or
  `HEAD~5..HEAD` all mean that reference's pinned commit, never the tip of the
  clone. A revision named explicitly, including the pin's own name, is passed to
  git as written.
- **One clone may back several entries** at different revisions, for example
  `net` and `net-next` pins from one kernel clone. Two entries that resolve to
  the same commit of the same clone are refused.
- **Point at the top of the repository.** The path must be the root of the
  work tree, or the git directory of a bare clone. A directory inside some other
  repository is refused, since git would otherwise serve every read from the
  enclosing one.
- **A nested clone or submodule inside the module repo does not work.** `git grep`
  and `git show <rev>:<path>` read the repository's own index, so only this
  setting makes a second tree reachable.
- **Linux project only.** Only the Linux review prompt explains reference trees
  to the model. With `[project].kind = "sashiko"` (or `--project sashiko`) the
  daemon refuses to start when `git.references` is non-empty, and a standalone
  worker ignores the list with a warning; both name the entries.
- An empty list reproduces single-repository behaviour exactly: no `repo`
  argument is advertised to the model, and neither block is added to the
  prompt. So does a patch for which every configured reference is dormant
  (see below).
- The list is read only where the full `Settings.toml` is loaded: the daemon
  and the review workers it spawns. `sashiko review` and `sashiko-cli local`
  load the local review settings, which have no `[git]` section, and have no
  flag for it, so reviews started that way are never grounded. The hidden
  `sashiko worker` subcommand takes two repeatable flags, merged by name over
  the configured list. They are the only way to set references alongside
  `--repo` or `--settings`, both of which load the local review settings, so
  `[git]` in the named file is ignored.
  - `--reference NAME=PATH[@REV]` replaces the path of entry `NAME` and drops
    its configured revision, which was chosen for a different clone; `@REV`
    pins the new one. Its `applies_to` is kept, since which patches a tree is
    for does not depend on where the clone lives. A `NAME` that is not
    configured is added only as `kernel`, and unscoped, because any other
    reference needs a description, and any scope an `applies_to`, which only
    `Settings.toml` can give. `PATH@REV` splits at the last `@`, so a
    revision containing `@` must be given with `--reference-revision`, and a
    path containing `@` must be followed by `@REV` (`@HEAD` pins the clone's
    current commit); otherwise the part after the path's own `@` is taken as
    the revision.
  - `--reference-revision NAME=REV` re-pins entry `NAME`, and wins over an
    `@REV` for the same name. An unknown `NAME` is an error.

#### Scoping a reference to part of the tree

A reference with `applies_to` is attached only to the reviews of patches that
touch a matching file; for every other patch it is *dormant*. In the driver-a
example above, an `eth/ionic/` patch is reviewed with the kernel alone, and
neither pays the tokens for rdma-core in every tool schema and the prompt nor
is invited to wander into it, while an `rdma/drv/` patch gets both.

- **Patterns** are globs relative to the root of the repository under review
  (not of the reference). `*` and `?` never match `/`, so they stay within one
  path component; only `**` crosses directories. The alternatives of `{a,b}`
  may themselves contain `/`, which is useful (`{rdma/drv,eth/ionic}/**`), and a
  character class such as `[!x]` is not guaranteed to exclude `/`, so do not
  rely on one to stop at a directory boundary. Matching is case-sensitive. A pattern
  without a `/` matches only top-level files, so write `**/ionic-abi.h` to match
  that name anywhere.
- **Refused patterns** stop the daemon and every worker at startup: an
  absolute pattern, one with a `..` or `.` component, one ending in `/` (write
  `dir/**`), one that does not compile, and an empty list (remove the key to
  attach the reference to every patch). Errors name the entry and the pattern's
  index, e.g. `git.references[rdma-core].applies_to[1] 'rdma/[drv': ...`.
- **Matching is per patch**, including within a series: each patch of a series
  is reviewed on its own and attaches its own references. Every file the patch
  touches counts, including both the old and the new path of a rename, so a
  file moving out of `rdma/drv/` still attaches rdma-core.
- **A dormant reference is invisible to the model**: it is not a `repo` value,
  not in the prompt's reference list and not in any tool description. If the
  model names it anyway, it is told the reference is configured but not
  attached to this review, which patterns it applies to, and which `repo`
  values are valid here. A name that is not configured at all gets the usual
  unknown-repo error.
- **Each patch logs one INFO line** with what it attached and why, and what it
  left dormant and why. The daemon's log may not be kept for long; the same
  decision is also recorded on the review (see the next item):

  ```
  Patch 1 references: attached kernel (every patch); dormant rdma-core (applies only to changes matching rdma/drv/**, eth/ionic/ionic_aux*, include/uapi/rdma/**; this patch touches none of them)
  Patch 2 references: attached kernel (every patch), rdma-core (rdma/drv/ionic/ionic_fw.h matches rdma/drv/**); dormant none
  ```

- **Recorded on each review.** Every daemon review stores, as JSON in
  `reviews.reference_repos` (API field `reference_repos` on reviews from
  `/api/patch`, `/api/patchset` and `/api/review`), each attached reference's
  name, kind, configured revision, resolved commit, why it was attached
  (`always`, or the first matching file and glob), and its read count, plus
  the dormant references with their `applies_to`. The worker sends the record
  as soon as the patch's references are decided and again, with read counts,
  in its final output, which it writes for a failed review as well as a
  successful one. Only a review whose worker is killed (a timeout or a
  cancellation) or exits without a final output keeps the record without read
  counts. A read is any tool call that named the reference, cache hits and
  calls git refused included. The review card shows it on one
  line, e.g. `References: kernel @ v6.18 (1a2b3c4d5e6f), 34 reads; rdma-core @
  origin/ionic-rdma (9f8e7d6c5b4a), 12 reads`, with the reason each was
  attached on hover. Dormant references are kept in the record and the API but
  not shown on the card, where they would be named on every patch outside their
  area. Reviews from before this was added, deployments with no references, and
  patches whose references were all dormant show no line. `sashiko review`
  and `sashiko-cli local` have no review row; their worker JSON carries the
  same object under `patches[].reference_repos`, with reads summed over the
  worker's in-process attempts.

- **The kernel rule.** The out-of-tree caveats appear only when a
  `kind = "kernel"` reference is attached to that patch. A kernel entry may be
  scoped, but a patch it is dormant for is framed as an in-tree review --
  `Fixes:` tags, lore and mainline paths are expected again -- and reads no
  kernel tree. Do not scope the kernel entry of an out-of-tree module.

#### `[[git.custom_remotes]]`

Optional array of additional git remotes to track.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | -- | Remote name. Required. |
| `url` | string | -- | Remote URL. Required. |
| `check_all_branches` | bool | -- | Try all branches as baselines. Required -- omitting it is a parse error, not a default of `false`. |
| `only_branches` | list | -- | Additional specific branches to try (optional). Additive: when `check_all_branches` is also true, these are appended to the full branch list rather than replacing it. |

Baselines are tried in order and the first one the series applies to wins.
A tree taken from a MAINTAINERS `T:` entry without a branch resolves to the
remote's HEAD. When that HEAD is a strict ancestor of the local mainline ref,
it is tried after every other candidate instead of in its usual place: it has
no commit that mainline lacks, and on some trees HEAD points to a commit dated
years ago.
Custom remotes are tried after any `base-commit:` trailer and the MAINTAINERS
subsystem heuristic, but **before** linux-next and mainline. A subsystem topic
branch is where a series was actually developed, whereas linux-next carries a
snapshot of that branch which lags by at least one daily build -- and shares no
SHAs with it once the maintainer rebases.

Use this to reach topic branches that the MAINTAINERS `T:` entry doesn't name.
For example, the NFSD entry lists a tree but no branch, so the only candidate it
yields is `cel/HEAD` (a symref to `cel/master`):

```toml
[[git.custom_remotes]]
name = "cel"
url = "git://git.kernel.org/pub/scm/linux/kernel/git/cel/linux.git"
check_all_branches = false
only_branches = ["nfsd-next", "nfsd-testing"]
```

Match `name` and `url` to a remote already in the repository when one exists.
`ensure_remote` rewrites the URL whenever the configured one differs, so an
`https://` URL here against a `git://` remote flips it back and forth on every
pass.

### `[review]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `concurrency` | integer | -- | Number of concurrent reviews. |
| `worktree_dir` | string | -- | Directory for git worktrees used during review. |
| `timeout_seconds` | integer | `3600` | Maximum time per review (seconds). |
| `max_retries` | integer | `3` | Retry count on transient failures. |
| `max_lines_changed` | integer | `10000` | Skip patches with more changed lines than this. |
| `max_files_touched` | integer | `200` | Skip patches touching more files than this. |
| `ignore_files` | list | `[]` | File patterns to skip during review (e.g. `MAINTAINERS`). |
| `email_policy_path` | string | `"email_policy.toml"` | Path to the email policy file. |
| `max_total_tokens` | integer | `5000000` | Maximum cumulative uncached tokens (input + output) per review. Cached tokens are excluded. Set to 0 to disable. |
| `max_total_output_tokens` | integer | `500000` | Maximum cumulative output tokens per review. Set to 0 to disable. |

### `[linux_bug]`

Controls pre-existing Linux kernel bug tracking and background analysis.
By default (`enabled = false`), pre-existing issues discovered during patch
reviews are ignored and the background bug worker is not started.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable pre-existing bug tracking, the `/api/bug/analyze` endpoint, and the background `BugWorker`. |
| `fix_check_enabled` | bool | `false` | Enable periodic verification of whether open bugs have been fixed in the upstream mainline branch (`master` for Linux, `main` for Sashiko). |
| `lease_ttl_seconds` | integer | `300` | How long a worker's claim on a bug stays valid without renewal. If the worker dies, the bug becomes claimable again once this elapses. |
| `max_attempts` | integer | `3` | How many analysis attempts a bug gets before it is abandoned. Abandoned bugs are never retried automatically. |
| `fix_check_interval_seconds` | integer | `21600` | Interval in seconds between periodic upstream fix checks against the mainline tree. Set to `0` to disable periodic checks. |
| `fix_check_batch_size` | integer | `50` | Maximum number of open bugs evaluated per upstream fix check cycle. |

### `[subsystems]`

Controls how patches and emails are categorized into subsystems for targeted reviews and specific email policies. By default, this section is empty, meaning the system relies on fallback heuristics (like identifying `@vger.kernel.org` addresses) to determine subsystems.

This feature is **globally active** and applies to both mailing list (NNTP) and Forge (Webhook) ingestion.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mapping` | list of objects | `[]` | A list of rules mapping a regular expression pattern to a subsystem name. |

Each mapping object in the list requires two fields:
* `pattern` (string): A regular expression used for matching.
* `name` (string): The resulting subsystem name if the pattern matches.

**How it works:**
- **For Git Forges (Webhooks):** The system applies the `pattern` against the **file paths** modified by a pull request (e.g., matching `^drivers/net/.*`).
- **For Mailing Lists (NNTP):** The system applies the `pattern` against the **To and Cc email addresses** of the incoming patch email.

When a patch is tagged with a subsystem, it can trigger subsystem-specific AI review rules (context loading) and specific email/embargo policies defined in `email_policy.toml`.

```toml
[subsystems]
mapping = [
    { pattern = ".*drivers/.*", name = "Drivers" },
    { pattern = ".*net/.*", name = "Networking" },
    { pattern = ".*fs/.*", name = "Filesystems" },
    { pattern = ".*mm/.*", name = "Memory Management" },
]
```

## email_policy.toml

Controls how Sashiko sends (or suppresses) review emails. See
[docs/examples/email_policy.toml](examples/email_policy.toml) for an
annotated example.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `defaults.reply_all` | bool | `false` | Allow sending to public mailing lists. |
| `defaults.reply_to_author` | bool | `false` | Send review to the patch author. |
| `defaults.cc_individuals` | bool | `false` | CC individual recipients (non-mailing-list) on review emails. |
| `defaults.mute_all` | bool | `true` | Suppress all email sending. |
| `defaults.cc` | list | `[]` | Static CC addresses. |
| `defaults.ignored_emails` | list | `[]` | Author addresses to ignore entirely. |
| `defaults.subject_prefixes` | list | `[]` | Subject prefix patterns to match for this scope. |
| `defaults.embargo_hours` | integer | -- | Hours to wait before publishing a review with findings. Clean reviews are released immediately after the complete patchset review succeeds. When a patch matches multiple subsystems, the shortest configured embargo wins. |
| `defaults.send_positive_review` | bool | `false` | Send email even when no issues are found. |

The email policy also supports per-subsystem overrides via
`[subsystems.<name>]` sections. Each subsystem section accepts the same
fields as `[defaults]`, plus:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `lists` | list | `[]` | Mailing list addresses that map to this subsystem. |
| `patchwork.enabled` | bool | `false` | Enable Patchwork integration for this subsystem. |
| `patchwork.api_url` | string | -- | Patchwork REST API URL (e.g. `https://patchwork.kernel.org/api/1.3`). Trailing slashes are stripped automatically. Invalid schemes are rejected with a warning. |
| `patchwork.token` | string | -- | Patchwork API token. Can also be set via `SASHIKO_PATCHWORK_TOKEN` env var (fills in where token is omitted in TOML). |
| `patchwork.email` | string | -- | Email address for email-based Patchwork notifications. |
| `patchwork.min_severity` | string | -- | Minimum finding severity to include in patchwork checks. Findings below this threshold are excluded. Accepts: `Low`, `Medium`, `High`, `Critical` (case-insensitive). Default: all findings included. |
| `patchwork.fail_severity` | string | `High` | Minimum severity of NEW findings that triggers the `fail` check state instead of `warning`. New findings at or above this threshold produce `fail`; below it produce `warning`. Pre-existing findings never affect the check state. |

### Author-only delivery

A subsystem can **track** a mailing list (its patches are ingested and
reviewed) while **not pinging** that list with the review email — sending
only to the patch author instead. This is useful for silent /
dashboard-only lists where individual contributors want direct reviews of
their own patches without spamming the whole list.

It is achieved with the existing flags, no extra key needed:

```toml
[subsystems.drm-intel]
lists = ["intel-xe@lists.freedesktop.org"]
reply_all = false          # never send to the public list
reply_to_author = true     # send the review to the patch author
cc_individuals = false     # drop non-list individual recipients
```

The list address is still matched via `lists` (so reviews happen), but
`reply_all = false` strips it from the outgoing recipients, leaving only
the author. Because each review targets exactly one author, this delivers
reviews to individual developers without pinging the list.

### Patchwork integration

Sashiko can report review results as
[checks](https://patchwork.readthedocs.io/en/latest/usage/overview/#checks)
on a Patchwork instance. Two delivery modes are available and can be
enabled simultaneously for the same subsystem.

**API mode** posts checks directly to the Patchwork REST API with
retry-queuing (3 attempts, exponential backoff). Requires a maintainer
API token. Note: Patchwork tokens grant full project-maintainer
permissions (state changes, delegation, etc.), not just check access.

```toml
[subsystems.net.patchwork]
enabled = true
api_url = "https://patchwork.kernel.org/api/1.3"
token = "your-api-token"   # or set SASHIKO_PATCHWORK_TOKEN env var
```

**Email mode** sends a structured notification email to a bot address.
A local script (such as
[pw_tools](https://github.com/mchehab/pw_tools)) parses the email and
posts the check. This avoids giving Sashiko a write token.

```toml
[subsystems.linux-media.patchwork]
enabled = true
email = "pw-bot@lists.example.org"
```

#### Severity filtering and check state mapping

By default, all findings are included in the patchwork check count.
Set `min_severity` to exclude findings below a threshold. When all
findings fall below the threshold, the check is posted as `success`.

The check state depends only on **new** findings (not pre-existing):

- `fail` -- new findings at or above `fail_severity` (default: `High`)
- `warning` -- new findings below `fail_severity`
- `success` -- no new findings (pre-existing findings are still
  shown in the description but do not affect the state)

The check description shows a per-severity breakdown with
pre-existing counts in parentheses, dropping zero-count severities.
For example: `Critical: 1 · High: 2 (1 pre-existing)`.

```toml
[subsystems.net.patchwork]
enabled = true
api_url = "https://patchwork.kernel.org/api/1.3"
min_severity = "Medium"    # exclude Low findings entirely
fail_severity = "High"     # High+ new findings = fail (default)
```

Edge case behaviors:

- Missing or null `preexisting` flag on a finding is treated as new
- When `min_severity` filters out all findings, the check is `success`
  with "Sashiko AI review found no regressions"
- When only pre-existing findings remain after filtering, the check
  is `success` but the description shows the pre-existing breakdown

#### Email notification format

When email mode is enabled, Sashiko sends a plain-text email with:

- **To**: the configured `patchwork.email` address
- **Subject**: `[sashiko-check] {status} - {patch_subject}`
- **Body** (one key-value pair per line):

```
msgid: <message-id>
status: success|warning
description: Sashiko AI review found N potential issue(s)
target_url: https://sashiko.dev/#/patchset/...
context: sashiko
```

Downstream tools can parse this format with simple line splitting.

## Environment variables

| Variable | Description |
|----------|-------------|
| `LLM_API_KEY` | API key for the configured LLM provider (universal fallback). |
| `GEMINI_API_KEY` | API key for Gemini (takes precedence over `LLM_API_KEY`). |
| `ANTHROPIC_API_KEY` | API key for Claude (takes precedence over `LLM_API_KEY`). |
| `OPENAI_API_KEY` | API key for OpenAI-compatible providers (takes precedence over `LLM_API_KEY`). |
| `ANTHROPIC_BASE_URL` | Override the Claude API base URL (for proxies). |
| `ANTHROPIC_VERTEX_PROJECT_ID` | GCP project ID for Vertex AI provider. |
| `CLOUD_ML_REGION` | GCP region for Vertex AI provider. |
| `SASHIKO_SERVER` | Override daemon URL for CLI commands. |
| `SASHIKO__*` | Override any Settings.toml value (e.g. `SASHIKO__AI__PROVIDER`). |
| `SASHIKO__FORGE__WEBHOOK_SECRET` | Override webhook secret from Settings.toml. Avoids storing the secret on disk. |
| `SASHIKO_PATCHWORK_TOKEN` | Patchwork API token. Fills in `patchwork.token` for enabled subsystems that have `api_url` set but no explicit token in TOML. |
| `NO_COLOR` | Disable ANSI color output. |
| `SASHIKO_LOG_PLAIN` | Use plain log format (no level/target/timestamp). |
