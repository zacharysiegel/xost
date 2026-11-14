#!/usr/bin/env bash
# Registers an xost account: writes its section to ~/.config/xost/config, stores
# the password (and optional TOTP seed) in the secr store, and creates the
# per-account data directory. Safe to re-run; it reuses an existing secr key
# and skips sections/secrets that already exist.
set -euo pipefail

secr_bin=$(command -v secr || true)
if test -z "${secr_bin}"; then
    echo "secr is not on PATH; install it first (cargo install secr or brew)." >&2
    exit 1
fi

config_dir="${HOME}/.config/xost"
config="${config_dir}/config"
store="${config_dir}/secrets.yaml"

read -r -p "Local label for this account (letters, digits, '-', '_'; e.g. nietzsche-dawn-daily; not the @handle) [default]: " account
account="${account:-default}"

account_valid=$(printf '%s' "${account}" | grep -E '^[A-Za-z0-9_-]+$' || true)
if test -z "${account_valid}"; then
    echo "account label must contain only letters, digits, '-', or '_' (no spaces); it becomes a path, a secret name, and a launchd label." >&2
    exit 1
fi

read -r -p "X handle (the @, without the @ sign): " handle
if test -z "${handle}"; then
    echo "handle is required." >&2
    exit 1
fi

read -r -p "Login identifier (email/phone/handle) [${handle}]: " username
username="${username:-${handle}}"

read -r -p "Path to Chrome binary (blank to auto-detect): " chrome

printf 'X password for %s: ' "${account}" >&2
read -rs password
echo >&2
if test -z "${password}"; then
    echo "password is required." >&2
    exit 1
fi

printf 'TOTP base32 seed (blank if the account has no authenticator 2FA): ' >&2
read -rs totp_seed
echo >&2

data_dir="${HOME}/.local/share/xost/${account}"
db="${data_dir}/queue.db"
profile="${data_dir}/chrome-profile"

mkdir -p "${config_dir}" "${data_dir}"

existing_key=""
if test -f "${config}"; then
    existing_key=$(grep -E '^[[:space:]]*key[[:space:]]*=' "${config}" | head -1 | sed -E 's/.*=[[:space:]]*//' || true)
fi

if test -n "${existing_key}"; then
    key="${existing_key}"
    echo "Reusing the existing secr key from ${config}." >&2
else
    key=$(secr key | tail -1)
fi

if test ! -f "${config}"; then
    echo "# xost config. chmod 600; holds the secr key. Comments on their own line." > "${config}"
    echo >> "${config}"
fi

secrets_present=""
if test -f "${config}"; then
    secrets_present=$(grep -E '^\[secrets\]' "${config}" || true)
fi

if test -z "${secrets_present}"; then
    {
        echo "[secrets]"
        echo "  store = ${store}"
        echo "  key = ${key}"
        echo
    } >> "${config}"
fi

account_present=""
if test -f "${config}"; then
    account_present=$(grep -F "[account \"${account}\"]" "${config}" || true)
fi

if test -n "${account_present}"; then
    echo "Account '${account}' already has a section in ${config}; leaving it unchanged." >&2
else
    {
        echo "[account \"${account}\"]"
        echo "  handle = ${handle}"
        echo "  username = ${username}"
        echo "  db = ${db}"
        echo "  profile = ${profile}"
        if test -n "${chrome}"; then
            echo "  chrome = ${chrome}"
        fi
        echo
    } >> "${config}"
fi

password_name="${account}_password"
existing_password=""
if test -f "${store}"; then
    existing_password=$(secr list --file "${store}" | grep -Fx "${password_name}" || true)
fi

if test -n "${existing_password}"; then
    echo "Secret '${password_name}' already in the store; leaving it unchanged." >&2
else
    secr encrypt --key "${key}" --name "${password_name}" "${password}" | sed $'s/\t/  /g' >> "${store}"
fi

if test -n "${totp_seed}"; then
    totp_name="${account}_totp_seed"
    existing_totp=""
    if test -f "${store}"; then
        existing_totp=$(secr list --file "${store}" | grep -Fx "${totp_name}" || true)
    fi

    if test -n "${existing_totp}"; then
        echo "Secret '${totp_name}' already in the store; leaving it unchanged." >&2
    else
        secr encrypt --key "${key}" --name "${totp_name}" "${totp_seed}" | sed $'s/\t/  /g' >> "${store}"
    fi
fi

chmod 600 "${config}"
if test -f "${store}"; then
    chmod 600 "${store}"
fi

echo
echo "Wrote account '${account}' to ${config}."
echo "Next: sign in once, then queue a tweet:"
echo "  xost --account ${account} login"
echo "  xost --account ${account} enqueue --body \"first post\""
