# xost

Posts queued tweets to one or more X accounts on a daily schedule by driving a headless Chrome session. Posts come from a per-account SQLite queue that rotates: each run posts the least-recently-posted enabled entry, wrapping to the start once all have gone out.

Each account is a `[account "<name>"]` section in `~/.config/xost/config`, selected with `--account <name>` (default `default`). The `<name>` is a local label you choose, restricted to letters, digits, `-`, and `_` (it names the config section, the data directory, the secret prefix, and the launchd job); it is not the @handle, which is a field inside the section. Accounts are isolated: separate queue db, separate Chrome profile, separate credentials. Authentication reuses a dedicated Chrome profile per account (isolated from your personal browser); the X session cookie lives there, not in this code. The password never reaches the command line or the database. It is read from a shared [`secr`](https://crates.io/crates/secr) store only when the tool needs to log itself back in after the session expires.

## Commands

- `xost --account <name> post` — post the next queued tweet. Headless. The scheduled daily run.
- `xost --account <name> login` — open your normal Chrome (not an automated session) on the account's profile to sign in by hand and refresh the session.
- `xost --account <name> enqueue [--heading "<text>"] --body "<text>" [--source-url <url>] [--order <n>]` — add a tweet. Order defaults to the end. A heading is posted before the body, joined by the account's `heading_separator` (default one space). `--source-url` is the record's link for the overflow post or the source post (see Threads under Behavior).
- `xost --account <name> diagnose` — open the account headless, report whether the session is valid, and save a screenshot to `/tmp/xost-diag.png`. Use it to check login state (e.g. after the yearly re-login).
- `xost --account <name> export-session` — print the account's X session cookies as JSON (run on a machine that's logged in). Transfers a session to a server without logging in there.
- `xost --account <name> import-session --from <file>` — load session cookies from a JSON file into this profile and confirm it's logged in.

`--account` defaults to `default`. `--config <path>` overrides the config location (default `~/.config/xost/config`).

## Config

One file, `~/.config/xost/config`, ssh-style. A shared `[secrets]` section plus one `[account "<name>"]` section per account. See `config.example`. `~/` is expanded in paths. Keep it `chmod 600` (it holds the `secr` key).

```
[secrets]
  store = ~/.config/xost/secrets.yaml
  key = <base64 key from `secr key`>

[account "default"]
  handle = your_handle
  db = ~/.local/share/xost/default/queue.db
  profile = ~/.local/share/xost/default/chrome-profile
```

Per-account files live under `~/.local/share/xost/<name>/`. Nothing sensitive goes in the repo.

## Setup

1. Build: `cargo build --release` (binary at `target/release/xost`).

2. Config: copy `config.example` to `~/.config/xost/config`, fill in the `[secrets]` section and one `[account "<name>"]` section, then `chmod 600 ~/.config/xost/config`.

3. Secrets. Mint one key and reuse it for every account's secrets (run in your own terminal; replace `<password>`):

       secr key

   Put the printed base64 key in the config's `[secrets] key`. Then store the account's password under the derived name `<account>_password`:

       secr encrypt --key <base64-key> --name default_password '<password>' >> ~/.config/xost/secrets.yaml
       chmod 600 ~/.config/xost/secrets.yaml

4. First sign-in:

       ./target/release/xost --account default login

   Complete the X login in the window that opens (including any SMS/email code), then close the window. `login` drives your normal Chrome, not an automated session, so X sees an ordinary browser. The session cookie persists in the profile for the headless `post` runs to reuse.

5. Queue some tweets:

       ./target/release/xost --account default enqueue --body "first post"

6. Schedule. Copy `launchd/ro.zach.xost.plist` to `~/Library/LaunchAgents/ro.zach.xost.<account>.plist`, replace `ACCOUNT_NAME` and the `REPLACE_WITH_*` paths, then:

       launchctl load ~/Library/LaunchAgents/ro.zach.xost.<account>.plist

   One job per account. Logs go to `~/Library/Logs/xost-<account>.log`.

## Running on a server (no desktop for manual login)

X blocks automated login, so the session has to be established in a real browser once. On a headless server you can't do that, so establish it on a desktop machine and transfer the session:

1. On a desktop (macOS with Chrome), register the account and sign in: `xost --account <name> login`.
2. Export the session: `xost --account <name> export-session > session.json`. This file contains the `auth_token` cookie — treat it as a secret.
3. Copy it to the server securely (`scp session.json server:`), then delete your local copy.
4. On the server, register the same account (config + `secr` password), then seed the profile: `xost --account <name> import-session --from session.json`. Delete `session.json` afterward.
5. Schedule `post` on the server. It reuses and refreshes the imported session.

The transferred cookie values are platform-independent (unlike the on-disk Chrome profile, whose encryption is OS-specific), so a macOS-exported session imports into a Linux server. When the session expires (~a year) or X invalidates it, repeat from step 2. A different server IP may trigger a one-time Cloudflare check; the `auth_token` still carries the login.

## Getting past X's automation blocks

X blocks automated browsers, so the headless `post` applies three measures (in `browser.rs`):

- Bypasses the x.com service worker via CDP (`Network.setBypassServiceWorker`); otherwise it intercepts and hangs the navigation.
- Overrides the User-Agent and `sec-ch-ua` client hints so the request doesn't advertise "HeadlessChrome", which x.com answers with HTTP 403.
- Launches `login`'s Chrome with `--password-store=basic --use-mock-keychain` so the session cookie is encrypted with the same basic store the headless CDP sessions use; otherwise the headless run can't decrypt the macOS-Keychain-encrypted cookie and appears logged out.

These are an anti-detection arms race: they work now, and X can change its checks later. The hard-coded `USER_AGENT` and client-hint versions in `browser.rs` should be bumped to match a current Chrome periodically.

## Behavior

- Already-posted-today: before posting, the tool reads the account's latest post time and skips if a post already exists today (UTC). Covers posts you make by hand.
- Crash safety (at-most-once): a row is locked before the publish click and unlocked on confirmation. A run that finds a row still locked from a previous run does not retry it; it notifies you so you can confirm on X whether it posted. A failure after the publish click is never retried.
- Threads: text weighing more than `thread_post_limit` (default 280, counted the way X counts) is posted as one thread, split at spaces and line breaks, never inside a word. Each post ends with ` k/n` (`thread_numbering = all_posts`, the default) or only the first does (`first_post`). Beyond `thread_max_posts` posts (default 9) the thread stops where that post's split falls and adds an unnumbered overflow post `<thread_overflow_label>: <source url>`. A record that needs that post fails unless both the record's source URL and the account's `thread_overflow_label` are set. A record that fits within `thread_max_posts`, including one that fits in a single post, instead ends with an unnumbered source post `<thread_source_label>: <source url>` when both are set, and with nothing extra otherwise.
- Failed records: a failure before the publish click is retried immediately in a fresh browser session, up to 3 attempts per run. A failure before X's composer is ready (browser launch, compose page, first editor) is an environment failure: it leaves the record untouched, and if the last attempt fails this way the run notifies you and the next run retries the same record. A later failure counts against the record. A record whose last attempt fails that way, or which cannot be split into a thread, gets `failed_at` set and is skipped on future runs; the run ends without posting another record. A sign-in failure does not mark the record failed. Requeue a failed record with `update tweet_queue set failed_at = null where id = <id>;`.
- Failures raise a macOS notification tagged with the account name.

## 2FA

- None: the tool logs itself back in from the stored password when the session expires. Unattended.
- Authenticator app (TOTP): add the base32 seed under `<account>_totp_seed` in the `secr` store (same `secr encrypt` command, different `--name`); the tool generates codes and stays unattended.
- SMS or email code: cannot be automated. When the session expires, `xost post` notifies you and exits without posting; run `xost --account <name> login` to sign in by hand.

## Caveats

- The X web selectors, login flow, and anti-automation measures are unofficial; X can change them without notice, which breaks posting until they are updated.
- Once-a-day posting from a residential address with a persisted session is low risk, not zero.
- The already-posted-today check reads the first `article time` on the profile, which is the pinned tweet if one exists; with a pinned tweet the check is unreliable.
- As a LaunchAgent the job fires only while you are logged into the desktop; it will not run while the machine sits at the login window.
