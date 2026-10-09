# GitHub Integration Setup

> **Experimental / Unsupported:** Git forge integration is experimental and
> unofficial. It is provided as-is, without guarantees of stability or
> correctness. Use it at your own risk. Breaking changes may occur without
> notice, and community support is best-effort only.

Complete guide for configuring Sashiko to review GitHub Pull Requests,
either automatically via webhooks or on request by pull request number (see
[Review a Pull Request by Number](#review-a-pull-request-by-number), which
needs no webhook and no GitHub App).

> **Note:** For general forge integration requirements and architecture, see [FORGE_SETUP.md](FORGE_SETUP.md). This guide covers GitHub-specific configuration.

## Quick Start (5 Minutes)

Get Sashiko reviewing GitHub Pull Requests quickly.

### Step 1: Configure LLM Provider

Set your API key:
```bash
# For Gemini
export LLM_API_KEY="your-gemini-api-key"

# For Claude
export ANTHROPIC_API_KEY="your-claude-api-key"
```

Verify `Settings.toml` has your LLM provider configured:
```toml
[ai]
provider = "gemini"  # or "claude"
model = "gemini-3.1-pro-preview"  # or "claude-sonnet-4-6"
```

### Step 2: Start Sashiko Server

```bash
cargo run --release
```

You should see output like:
```
Server running on http://127.0.0.1:8080
```

**Verify it's running:**
```bash
curl http://localhost:8080/
```

### Step 3: Test with a Pull Request

#### Option A: Test with Real GitHub PR

Set `forge.github_repository` to the repository the pull request is opened
against, make sure `gh auth status` passes as the user that runs the daemon,
and ask for the pull request by number:
```bash
cargo run --bin sashiko-cli -- submit '#12345'
```

Replace `12345` with a PR number. See
[Review a Pull Request by Number](#review-a-pull-request-by-number) for
what this needs and what it reports.

#### Option B: Test with Synthetic Webhook

Send a test webhook payload to verify the endpoint:
```bash
./scripts/test_github_webhook.sh
```

#### Option C: Test with Local Commits

Review your own local changes:
```bash
# Review last 3 commits
cargo run --bin sashiko-cli -- submit HEAD~3..HEAD
```

### Step 4: Monitor Progress

Open your browser to the web UI:
```
http://localhost:8080/
```

You'll see:
- Review queue status
- Current review progress
- Completed reviews with findings

### Step 5: Configure Webhook (Optional)

To automatically review PRs when they're opened on GitHub:

1. Go to your GitHub repository → Settings → Webhooks
2. Add webhook:
   - **URL:** `http://your-server:8080/api/webhook/github`
   - **Content type:** `application/json`
   - **Events:** Pull requests only
3. For local development, use ngrok or SSH tunnel:
   ```bash
   # Using ngrok
   ngrok http 8080
   # Use the ngrok URL in webhook configuration
   ```

## Prerequisites

- Sashiko running in server mode (daemon)
- Server accessible from GitHub (public URL or tunneling solution like ngrok)
- Admin rights on GitHub repository (to configure webhooks)
- Sashiko configured with valid LLM API credentials

## Configuration

### 1. Configure Server Settings

Sashiko's webhook endpoints are available by default when running the daemon. The server configuration in `Settings.toml`:

```toml
[server]
host = "127.0.0.1"
port = 8080
```

**Security Note:** By default, Sashiko only accepts webhook requests from `localhost` for security. To accept webhooks from GitHub's servers, you have two options:

**Option A: SSH Tunnel (Recommended for testing)**
```bash
# On your development machine
ssh -R 8080:localhost:8080 your-public-server.com

# GitHub webhook URL would be:
# http://your-public-server.com:8080/api/webhook/github
```

**Option B: Allow all submit (Use with caution)**
```bash
# Start Sashiko with the unsafe flag
cargo run --release -- --enable-unsafe-all-submit
```

**⚠️ WARNING:** Option B allows anyone who can reach your server to trigger reviews. Only use this in trusted networks or behind authentication layers (reverse proxy with auth, firewall rules, etc.).

### 2. Set up GitHub Webhook

1. Navigate to your repository on GitHub
2. Go to **Settings** → **Webhooks** → **Add webhook**
3. Configure the webhook:
   - **Payload URL:** `http://your-server:8080/api/webhook/github`
     - Replace `your-server` with your actual server address
     - Use port `8080` (or your configured server port)
   - **Content type:** `application/json`
   - **Secret:** Enter a strong random token (generate with `openssl rand -hex 32`).
     Add the same value as `webhook_secret` in Sashiko's `Settings.toml`.
     See the [Webhook Security Guide](WEBHOOK_SECURITY.md#github-hmac-setup) for details.
   - **Which events would you like to trigger this webhook?**
     - Select: **Let me select individual events**
     - Check: ✓ **Pull requests**
     - Uncheck all other events
   - **Active:** ✓ Enabled

4. Click **Add webhook**

### 3. Verify Webhook Delivery

After creating the webhook, GitHub will immediately send a test `ping` event:

1. In GitHub webhook settings, click on your newly created webhook
2. Navigate to the **Recent Deliveries** tab
3. You should see a `ping` event with a green checkmark
4. If the delivery failed, check:
   - Server is running: `curl http://localhost:8080/`
   - Firewall allows incoming connections
   - URL is correct and accessible from the internet

### 4. Test the Integration

**Option A: Use the test script**
```bash
./test_github_webhook.sh
```

This sends a synthetic webhook payload to verify the endpoint is working.

**Option B: Trigger a real PR review**
```bash
sashiko-cli submit '#123'
```

The daemon resolves the pull request with gh and queues its current head
revision, exactly as a webhook delivery for that head would. See
[Review a Pull Request by Number](#review-a-pull-request-by-number).

**Option C: Open a real Pull Request**

The most authentic test - simply open a new PR on your repository. Sashiko should:
1. Receive the webhook
2. Log the PR details
3. Queue the review
4. Fetch the commits
5. Start the AI review process

Check the web UI at `http://localhost:8080/` to see the review progress.

## Review a Pull Request by Number

An operator can ask the daemon to review a pull request by its number. This
needs no GitHub App, no webhook and no public address: the daemon looks the
pull request up with the GitHub CLI (`gh`) on its own host and fetches the
commits itself. It is the way to review pull requests on a daemon that
GitHub cannot reach.

### What it needs

1. **gh, logged in as the user that runs the daemon.** Run `gh auth status`
   as that user; it must report a logged-in account that can read the
   repository. For GitHub Enterprise, log in to that host with
   `gh auth login --hostname <host>`. The daemon runs
   `gh pr view <N> --repo=<host>/<owner>/<name> --json
   number,title,url,baseRefOid,headRefOid` with a 60-second limit and
   prompts disabled. Because the repository is named on the command line,
   gh never reads the daemon's git remotes.
2. **The repository, in `Settings.toml`.** Pull request numbers belong to
   the repository the pull requests are opened against, not to a fork, so
   name that one:

   ```toml
   [forge]
   provider = "github"
   github_repository = "example-org/driver-a"   # or "host/owner/name" for GitHub Enterprise
   ```

   Each part may hold only letters, digits, `_`, `.` and `-`; any other
   value stops the daemon at startup with an error naming the setting. One
   daemon reviews pull requests of one repository by number. The setting
   is never guessed from a git remote, because the daemon's repository also
   holds remotes whose URLs come from requests. With
   `provider = "gitlab"`, review by number is refused: it needs gh, which
   is GitHub only.
3. **The right to ingest.** `sashiko-cli` on the daemon's host sends the
   local operator token (`.sashiko-local-token`, next to the database)
   without being asked, which grants both rights below. Any other caller
   needs a signed-in address in `server.acl.ingest`, and for
   `rerun --pr` in `server.acl.review` as well (see
   [configuration.md](configuration.md)). A request is never trusted for
   the address it comes from.
4. **Read access to the commits over https** (next section). A public
   repository needs nothing more.

`forge.enabled`, `forge.webhook_secret` and `forge.api_token` play no part.

### Pull requests from forks: https credentials

The daemon fetches each revision from
`https://<host>/<owner>/<name>.git`, built from `forge.github_repository`,
which is the same URL a GitHub webhook sends. It fetches through a remote
of its own (`fetcher-<hash>`), never through `origin`, so an SSH `origin`
that works proves nothing about this fetch. It first asks for the base and
head commits by SHA, which on github.com reaches the head of a pull request
from a fork, even one whose fork was deleted. If that fetch fails, it
fetches `refs/pull/<N>/head` from the base repository, and then every
branch.

For a private repository, the daemon's user therefore needs https read
credentials for that URL. Either let git use gh's login:

```bash
gh auth setup-git
```

or send the https URL over SSH with a key that can read the repository:

```bash
git config --global url."git@github.com:".insteadOf "https://github.com/"
```

Check it as the daemon's user; it must print a commit id and the ref
without asking for a password:

```bash
git ls-remote https://github.com/<owner>/<repo>.git refs/pull/<N>/head
```

When the forge refuses both the fetch by SHA and the pull request refs, the
daemon logs a WARN that names the pull request, the URL and these two
fixes.

### Asking for a review

```bash
# Review the current head of pull request 967
sashiko-cli submit '#967'
sashiko-cli submit https://github.com/example-org/driver-a/pull/967

# Review it again, even if that head was already reviewed
sashiko-cli rerun --pr 967
```

`submit` recognises `#N`, `pr/N`, `pr#N`, `pr-N` and `prN` (in any case)
and pull request URLs; quote `#N`, since the shell reads an unquoted `#` as
the start of a comment. A URL must name the configured repository, or the
request is refused with both names. A bare number is still a git revision.
See [sashiko-cli.md](sashiko-cli.md#submit) for the details.

The daemon resolves the number to the pull request's current base and head
and queues that head revision through the same path a webhook delivery
takes: the same patchset, the same `#N: title` or `#N [vK]: title` subject,
the same `<repo>-<N>` slug, and the same cancellation of older revisions
still in progress. A webhook delivery and a request by number for the same
head meet at one patchset. Two things differ, because the operator named
the pull request: the webhook's action filter and its Dependabot skip do
not apply, and a revision that is already waiting, under review or
finished is not fetched again.

### What the daemon answers

| Outcome | When | Message |
| --- | --- | --- |
| `queued` | The head is new here, or its last attempt Failed, was Cancelled, was Partial or failed to apply. | `Queued PR #967 (head 1a2b3c4d) as patchset 41`, or `PR #967 head 1a2b3c4d had Failed as patchset 41, v3; queued again` |
| `already_in_progress` | The head is still being fetched (the fetch is sent again, which recovers one lost to a daemon restart), or is waiting for or under review. | `PR #967 head 1a2b3c4d is already being reviewed as patchset 41 (Pending)` |
| `already_finished` | `submit`, and the head was Reviewed or Skipped. | `PR #967 head 1a2b3c4d already finished as patchset 41 (Reviewed); use rerun --pr 967 to review it again` |
| `rerun_queued` | `rerun --pr`, and the head had finished. | `Rerun of PR #967 queued as patchset 41` |

`rerun --pr` on a head that has not finished does what `submit` does.

A refused request gets an HTTP status and one line saying why:

| Status | Why |
| --- | --- |
| 403 | The server is read-only, or the caller lacks Ingest (or Review, for `rerun --pr`). |
| 400 | The number is below 1, or a URL names another repository. |
| 422 | `forge.github_repository` is not set, or `forge.provider` is `gitlab`. |
| 502 | gh could not resolve the number: it is not installed, not logged in, took over 60 seconds, failed (its first error line is quoted), or answered with something other than the pull request asked for. The reply names the newest revision of that pull request already known here, if any, with the `sashiko-cli rerun <id>` that reviews it again; it is not rerun on its own. |
| 409 | A newer revision of the pull request is already known here, so this head would be cancelled rather than reviewed; or the finished head changed state before `rerun --pr` could rerun it. |

The route behind both commands is `POST /api/pr/review`, with a JSON body
`{"number": 967, "mode": "review_head"}` (`"rerun_head"` for a rerun; an
optional `"repo"`, as `"owner/name"` or `"host/owner/name"`, must name
the configured repository) and the operator token as a bearer token. It answers
`{"outcome", "patchset_id", "number", "head", "previous_status",
"message"}`, which `sashiko-cli --format json` prints as it is.

### Opening one revision of a pull request

Each revision of a pull request is its own patchset, and three addresses
open one: on the page (`#/patchset/<address>`), with
`sashiko-cli show <address>`, and on `GET /api/patch` and
`GET /api/patchset`.

| Address | Opens |
| --- | --- |
| `<repo>-<N>`, e.g. `driver-a-967` | The newest revision. When a newer one arrives, this address moves to it. |
| `<repo>-<N>-vK`, e.g. `driver-a-967-v2` | Revision K, once a newer revision has taken `<repo>-<N>`. |
| `<repo>-<N>-<sha>`, e.g. `driver-a-967-1a2b3c4` | The revision whose head commit starts with `<sha>`: 7 to 40 hex characters, in any case. It never moves, so it is the one to share. |

`<repo>` is the repository name without its owner. The head is the one
the revision was queued with, so a revision whose fetch failed or was
cancelled still opens. A stored slug always wins: if a repository is
itself named like `<repo>-<N>`, its own slugs keep their meaning. An
address that finds no single revision answers 404 with one line saying
why: no revision of that pull request is known, none has a head starting
with the given characters, or more than one has (give more of the commit).

The review comment Sashiko posts on a pull request links to the revision
it describes with this address, using the first 12 characters of the
head, so an older comment keeps opening its own review after the pull
request is pushed again. A revision whose head is not known from the id
it was queued with is linked by its stored slug instead.

## Troubleshooting

### Webhook not received

**Symptoms:** No logs in Sashiko output when PR is opened

**Solutions:**
- Verify server is running: `curl http://localhost:8080/`
- Check firewall allows incoming connections on port 8080
- Verify webhook URL is accessible from internet (use ngrok or similar for testing)
- Check GitHub webhook delivery status in repository settings

### Permission denied (403 Forbidden)

**Symptoms:** GitHub shows delivery failed with 403 status

**Cause:** Sashiko could not authenticate the webhook. A tunnel or proxy does
not help by itself: the source address is not consulted, so a forwarded
request is treated exactly like any other.

**Solutions:**
1. **Recommended:** Set `webhook_secret` and configure the same secret in GitHub
2. **Quick test:** Run with `--enable-unsafe-all-submit` flag
3. **Production:** Reverse proxy with TLS, plus the signature check above

### Webhook received but review not starting

**Symptoms:** Logs show webhook received but no review begins

**Solutions:**
- Check LLM API key is configured: `echo $LLM_API_KEY`
- Verify git repository is accessible: check `git.repository_path` in `Settings.toml`
- Look for errors in Sashiko logs
- Check that the commit hash exists in your configured repository

### Review fails immediately

**Symptoms:** Review status shows "failed" in web UI

**Solutions:**
- Check Sashiko logs for specific error messages
- Verify git repository has the referenced commits
- Ensure LLM provider is accessible and API key is valid
- Check disk space in `worktree_dir` directory

## Security Considerations

Sashiko verifies GitHub webhook signatures using HMAC-SHA256 when
`webhook_secret` is configured. This mitigates forged webhook requests that
could trigger unauthorized reviews.

For production deployment instructions, reverse proxy examples, and a
complete security checklist, see the
[Webhook Security Guide](WEBHOOK_SECURITY.md).

## Advanced Configuration

### Custom Port

Modify `Settings.toml`:
```toml
[server]
host = "0.0.0.0"  # Listen on all interfaces
port = 8080       # Custom port
```

Then update webhook URL: `http://your-server:8080/api/webhook/github`

### Multiple Repositories

Sashiko can handle webhooks from multiple repositories. Simply add the same webhook configuration to each repository you want to monitor.

**Note:** All repositories must be accessible from the git repository configured in `Settings.toml` or Sashiko will fail to fetch the commits.

## Webhook Payload Reference

Sashiko processes the following fields from GitHub's `pull_request` webhook:

```json
{
  "action": "opened",
  "pull_request": {
    "number": 123,
    "title": "Fix memory leak in driver",
    "html_url": "https://github.com/owner/repo/pull/123",
    "head": {
      "sha": "abc123..."
    },
    "base": {
      "sha": "def456..."
    }
  }
}
```

Supported actions: `opened`, `reopened`, `synchronize` (new commits
pushed), `ready_for_review` (a draft marked ready). Every other action is
acknowledged and skipped.

## See Also

- Quick start content is now in the top section of this document
- [FORGE_SETUP.md](FORGE_SETUP.md) - General forge integration guide
- [GITLAB_SETUP.md](GITLAB_SETUP.md) - GitLab integration setup
- [README.md](../README.md) - Main project documentation
- [Settings.toml](../Settings.toml) - Configuration reference

## Getting Help

- **Mailing List:** sashiko@lists.linux.dev ([lore archive](https://lore.kernel.org/sashiko))
- **GitHub Issues:** [Report bugs or request features](https://github.com/sashiko-dev/sashiko/issues)
