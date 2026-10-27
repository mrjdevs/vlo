# Deploying VLO on cPanel / StackCP

VLO is a single static binary. It does not need PHP, Node.js, Passenger or a
build toolchain on the host — Apache only has to forward HTTP requests to a
process you start over SSH.

This guide targets the account already prepared for VLO:

| Item | Value |
|---|---|
| Host | StackCP (20i) |
| SSH | `anjumtips.com.pk@ssh.gb.stackcp.com:22` |
| Site | `https://vlo.anjumtips.com.pk` |
| Document root | `/home/sites/1a/e/eb23ffc95e/public_html/vlo` |
| Panel | SSH only — no CGI, no Node.js/Passenger |
| Status (2026-10-27) | SSH still closes before the key exchange (`kex_exchange_identification`). Nothing has been executed on the host yet. |

Read section 1 before building anything: two of the three checks decide whether
this architecture can work at all on this package.

---

## 1. Feasibility checks (go / no-go)

Run these first, in order. Do not promise a working deployment before all three
pass.

### 1.1 SSH actually connects

```bash
ssh -i ~/.ssh/id_ed25519_vlo_cpanel anjumtips.com.pk@ssh.gb.stackcp.com
```

If you get `kex_exchange_identification: Connection closed by remote host`
before any key is offered, the problem is not your key. It is package-level:

- confirm SSH is enabled for the package,
- confirm the key fingerprint `SHA256:dJRc6NCOVLDzLKlJHQGfriPE5AMVcTGCOv91ev0BLxY` is the one listed in the panel,
- ask support to allowlist your egress IP,
- open a ticket quoting the exact error.

No SSH means no execution path. Stop here.

### 1.2 A background process survives logout

```bash
nohup sleep 600 >/dev/null 2>&1 &
echo $!
# log out, log back in
pgrep -af "sleep 600"
```

Many shared hosts reap user processes when the session ends or after a short
TTL. If the process is gone, VLO cannot stay up on this package (section 6.3
covers the cron workaround, which fails on hosts that kill processes on every
cron run).

### 1.3 Apache can proxy to a local port

Start a throwaway listener and proxy it:

```bash
python3 -m http.server 3000 --bind 127.0.0.1 &

cd /home/sites/1a/e/eb23ffc95e/public_html/vlo
printf 'RewriteEngine On\nRewriteRule ^vlo-ping$ http://127.0.0.1:3000/ [P,L]\n' > .htaccess

curl -i https://vlo.anjumtips.com.pk/vlo-ping
```

A `200` with a directory listing means the proxy works. A `500`, `403` or a
Proxy Error means `mod_proxy` / `RewriteRule [P]` is disabled or
`AllowOverride` forbids it. Kill the Python server afterwards; the `.htaccess`
rule is replaced in section 6.2.

If 1.3 fails, this plan cannot work on shared hosting regardless of SSH — the
fallback is a small VPS, or asking the host to enable Node.js/Passenger.

The same topology has already been proven locally with Laragon Apache 2.4.66
(`ProxyPreserveHost On`, `ProxyPass / http://127.0.0.1:3000/`): page guards,
APIs, static assets, CSRF and cart PUT all behaved identically through the
proxy.

---

## 2. Build the release bundle locally

The host never compiles anything. Build on your machine:

```bash
cd D:\Programming\vlo

# static Linux binary (verified profile: musl + stripped, ~7.8 MB)
cargo build --release --target x86_64-unknown-linux-musl

# fail fast on broken templates / components / API namespaces
target\release\vlo.exe check --strict

# regenerate the production page cache
target\release\vlo.exe build
```

Produce the MySQL schema dump if it is not current:

```bash
target\release\vlo.exe schema --driver mysql --out schema-mysql.sql
```

Package exactly this tree — no `target/`, no `.git/`, **no `.env`**:

```
vlo                     # target/x86_64-unknown-linux-musl/release/vlo
pages/                  # API definitions are parsed from pages/api/ at startup
modules/                # manifests + module assets
components/             # keep the tree complete; cheap
layouts/
.vlo/build/             # output of `vlo build`, including routes.json and static/
storage/uploads/        # writable at runtime
schema-mysql.sql        # the dialect you will import/apply
```

`.vlo/build/routes.json` is mandatory: `vlo serve` refuses to serve guarded
pages when it is missing or invalid. `public/` is build-time only — it is
copied into `.vlo/build/static/` by `vlo build`.

On Windows, create the archive from Git Bash:

```bash
tar -czf vlo-deploy.tgz \
  -C target/x86_64-unknown-linux-musl/release vlo \
  -C /d/Programming/vlo pages modules components layouts .vlo/build storage \
  schema-mysql.sql
```

---

## 3. Database

MySQL is the right choice on shared hosting: cPanel manages it, quotas and
backups are handled, and it survives process restarts. SQLite works for a
demo but is a single file with weaker concurrency guarantees.

### 3.1 MySQL (recommended)

1. cPanel → **MySQL Databases**: create database and user, grant ALL on the
   database.
2. Import the dump — phpMyAdmin → *Import* → `schema-mysql.sql`, or:

   ```bash
   mysql -h 127.0.0.1 -u <db_user> -p <db_name> < schema-mysql.sql
   ```

3. Connection string:

   ```ini
   DATABASE_URL=mysql://<db_user>:<url_encoded_password>@127.0.0.1:3306/<db_name>
   DB_DRIVER=mysql
   DB_SCHEMA=schema-mysql.sql
   ```

`DB_SCHEMA` matters: at startup VLO executes every non-`INSERT` statement in
that file. Shipping the default `schema.sql` (SQLite dialect) to a MySQL host
produces a stream of failed statements. Either point `DB_SCHEMA` at the MySQL
dump (statements are `CREATE TABLE IF NOT EXISTS`, so re-running is safe) or
remove the SQLite file from the bundle.

### 3.2 SQLite (fallback)

```ini
DATABASE_URL=sqlite:///home/sites/1a/e/eb23ffc95e/vlo-app/vlo_app.db
DB_DRIVER=sqlite
DB_SCHEMA=schema.sql
```

The directory must be writable by the account. The `-wal` and `-shm` files are
created next to the database, so file permissions and backups have to include
them.

---

## 4. Environment file

Place `.env` **next to the binary** (VLO loads it from the working directory);
never inside a web-accessible path. `chmod 600 .env`.

```ini
APP_ENV=production
APP_NAME=VLO
APP_URL=https://vlo.anjumtips.com.pk

HOST=127.0.0.1
PORT=3000

VLO_FEATURES=auth,database,api,modules,files,flash,pagination,directives,sse,email

DATABASE_URL=mysql://<db_user>:<url_encoded_password>@127.0.0.1:3306/<db_name>
DB_DRIVER=mysql
DB_SCHEMA=schema-mysql.sql

# at least 32 characters; generate with: openssl rand -base64 48
SESSION_SECRET=<secret>
AUTH_COOKIE_SECURE=true
VLO_TRUST_PROXY=true

VLO_MAX_UPLOAD_MB=10

# leave unset in production
# VLO_DEBUG=
# VLO_DEBUG_SERVE=
```

Notes:

- `HOST=127.0.0.1` keeps the app unreachable from the internet except through
  Apache. It is the default, but set it explicitly.
- `VLO_TRUST_PROXY=true` makes rate limiting and logging read
  `X-Forwarded-For`, which Apache's proxy sets. Only enable it behind the proxy.
- `AUTH_COOKIE_SECURE=true` is required under HTTPS or browsers will drop the
  session cookie. It is inferred automatically when `APP_URL` starts with
  `https://`, but set it explicitly so a typo in `APP_URL` cannot silently
  downgrade the cookie.
- The `AUTH_USER_*` column mappings have defaults matching the shipped schema.
  Override them only if your `users` table uses different column names.
- Optional mail settings (`MAIL_*`) are documented in `.env-example`.
- `VLO_FEATURES` is validated at startup: `auth`, `api` and `jobs` require
  `database`, and an unknown feature name aborts the process with a clear
  error instead of being ignored.

---

## 5. Upload and lay out the files

Keep application files **outside** the document root and leave only the proxy
rules inside it:

```
/home/sites/1a/e/eb23ffc95e/
├── vlo-app/                  # binary, .env, pages/, .vlo/build/, storage/, schema
└── public_html/vlo/          # document root for vlo.anjumtips.com.pk
    └── .htaccess             # proxy to 127.0.0.1:3000
```

That way a broken rewrite cannot expose `.env`, the database or logs.

Upload (SSH required):

```bash
scp vlo-deploy.tgz anjumtips.com.pk@ssh.gb.stackcp.com:~/vlo-app/
ssh anjumtips.com.pk@ssh.gb.stackcp.com
cd ~/vlo-app && tar -xzf vlo-deploy.tgz
chmod +x vlo
chmod 600 .env
mkdir -p storage/uploads logs
chmod 750 storage
```

The panel's **Git Version Control** and **FTP** can deliver files without SSH,
but neither can execute the binary — they are only useful in combination with
SSH, or for updating files the app reads.

---

## 6. Start and keep it running

### 6.1 First run

```bash
cd ~/vlo-app
./vlo check --strict            # validate the uploaded templates
nohup ./vlo serve >> logs/serve.log 2>&1 &
echo $! > logs/serve.pid
sleep 2
curl -s http://127.0.0.1:3000/healthz
```

`vlo serve` binds `HOST:PORT` (default `127.0.0.1:3000`) and requires an
existing `.vlo/build/`. It does not build pages at runtime.

### 6.2 Enforce the proxy

`.htaccess` in `public_html/vlo`:

```apache
RewriteEngine On

# never let dotfiles, dumps or logs fall through to the proxy
RewriteRule (^|/)\. - [F]
RewriteRule \.(db|sql|log)$ - [F]

# forward everything else to VLO
RewriteRule ^(.*)$ http://127.0.0.1:3000/$1 [P,L,QSA]
```

`ProxyPreserveHost On` is server-level configuration and usually unavailable in
`.htaccess`. VLO builds redirects from `APP_URL`, so keep `APP_URL` correct
rather than relying on the forwarded `Host` header.

### 6.3 Survive reboots and logouts

If section 1.2 passed, a `nohup`/`setsid` process persists. Add a cPanel cron
as a watchdog (replace paths with the account's real path):

```
*/5 * * * * cd /home/sites/1a/e/eb23ffc95e/vlo-app && (kill -0 "$(cat logs/serve.pid 2>/dev/null)" 2>/dev/null || { nohup ./vlo serve >> logs/serve.log 2>&1 & echo $! > logs/serve.pid; })
```

Keep `logs/serve.log` under the app directory and prune it (cPanel cron can run
`find logs -name '*.log' -mtime +14 -delete`). Never write logs into
`public_html`.

---

## 7. Verification checklist

From the host:

```bash
curl -s  http://127.0.0.1:3000/healthz          # healthy JSON
curl -sI http://127.0.0.1:3000/                 # 302/303 to /login when guarded
```

From the public URL:

1. `https://vlo.anjumtips.com.pk/healthz` → 200.
2. `/` → login redirect; `/login` renders with working styles (`/static/...`).
3. Login as a known account; the session cookie is marked `Secure`.
4. Guarded page (`/editor` or `/admin`) → 200 for the right role, `403` for the wrong one.
5. A public API (`/api/products`) → 200; an anonymous write → `401`/`403`.
6. A form POST without CSRF token → 403, with token → succeeds.
7. Upload a small file (under `VLO_MAX_UPLOAD_MB`) and fetch it from
   `/storage/uploads/...`.
8. Security headers present on HTML responses.

---

## 8. Updates and rollback

```bash
cd ~/vlo-app

# upload the new tarball, extract to a staging dir first
mkdir -p staging
tar -xzf vlo-deploy-new.tgz -C staging

# swap: keep the previous binary only until health passes
mv vlo vlo.prev
cp staging/vlo ./vlo && chmod +x vlo

# if pages or API definitions changed, replace pages/ and .vlo/build/ too
# (keep the old copies until verification finishes)

pkill -f "./vlo serve" || kill "$(cat logs/serve.pid 2>/dev/null)" 2>/dev/null
sleep 1
nohup ./vlo serve >> logs/serve.log 2>&1 &
echo $! > logs/serve.pid
sleep 2
curl -s http://127.0.0.1:3000/healthz && curl -sI https://vlo.anjumtips.com.pk/healthz
```

Rollback: stop the process, restore `vlo.prev`, restart. If pages were
replaced, restore the previous `pages/` and `.vlo/build/` at the same time — a
new binary with an old build cache, or vice versa, is the most common source
of confusing 500s.

Run `vlo check --strict` before every upload; it catches the missing component
and stale API-namespace mistakes that otherwise only appear at request time.

---

## 9. Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `kex_exchange_identification: Connection closed` | SSH disabled at package level or IP allowlist | Enable SSH in the panel, allowlist the IP, open a ticket |
| Public URL returns 500 / Proxy Error | `mod_proxy` or `RewriteRule [P]` blocked | Ask the host to enable it; otherwise use a VPS |
| `curl 127.0.0.1:3000` works, public URL does not | `.htaccess` not read (wrong docroot) or rewrite loop | Confirm document root points at `public_html/vlo`, check `AllowOverride` |
| `500 - Build manifest missing or invalid` | `.vlo/build/routes.json` not uploaded | Re-upload `.vlo/build/`; rerun `vlo build` locally |
| `Address already in use` | Previous process still bound | `pgrep -af "vlo serve"`, kill the old PID, restart |
| Everyone is role `User` | `AUTH_USER_*` mappings do not match the table | Align the mappings with the schema |
| Login succeeds then immediately logs out | session cookie not secure/HTTPS or `SESSION_SECRET` missing | Set `AUTH_COOKIE_SECURE=true`, a ≥32-char secret, correct `APP_URL` |
| Random statements fail at startup | SQLite `schema.sql` applied to MySQL | Set `DB_SCHEMA=schema-mysql.sql` or remove `schema.sql` |
| `permission denied` on uploads | `storage/` not writable | `chmod 750 storage`, ensure ownership |
| Process dies after logout | host reaps user processes | Use the cron watchdog (6.3) or move to a VPS |

---

## 10. Security notes

- The `.env` file never goes into the repo, the tarball or the document root.
- Keep the app directory outside `public_html`; the rewrite is the only link
  between Apache and the process.
- Bind to `127.0.0.1` only. A public port would bypass Apache's TLS and
  `X-Forwarded-For` handling.
- Do not use `vlo cgi` — it is still a stub and lacks API/auth/static handling.
- Rotate `SESSION_SECRET` only with a planned logout of everyone.
- Delete temporary FTP accounts and test users after verification.

---

## 11. Not yet proven on this host

1. SSH connects and stays usable.
2. `.htaccess` proxying works on the StackCP package.
3. Background processes survive logout and reboot.
4. MySQL connectivity from the binary (the local MySQL test used Laragon).

Until 1–3 are confirmed, treat the deployment as prepared, not delivered.
