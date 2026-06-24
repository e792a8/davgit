# davgit — Pure-Rust Git WebDAV Server

Serves a remote Git repository as WebDAV server.

## Features

- **Read**: GET, HEAD, PROPFIND (directory listings as multistatus XML)
- **Write**: PUT (create/update files), MKCOL (create directories), DELETE
- **Copy/Move**: COPY and MOVE for files and directories
- **Authentication**: SSH agent (`$SSH_AUTH_SOCK`) or PEM key files (`--ssh-key` or default paths under `~/.ssh/`)
- **Pure Rust**: No external dependencies other than the Rust standard library

## Usage

```bash
davgit \
  --remote-url 'git@github.com:user/repo.git' \
  --branch main \
  --port 18080
```

Then mount `http://localhost:18080/` as a WebDAV drive in your file manager or use `curl`:

```bash
# List files
curl -X PROPFIND http://localhost:18080/ -H "Depth: 1"

# Read a file
curl http://localhost:18080/README.md

# Create a file
curl -X PUT -d "hello" http://localhost:18080/hello.txt

# Delete a file
curl -X DELETE http://localhost:18080/hello.txt

# Create a directory
curl -X MKCOL http://localhost:18080/docs/

# Copy a file
curl -X COPY http://localhost:18080/README.md \
  -H "Destination: /README-copy.md"
```

### CLI Options

| Flag | Default | Description |
|---|---|---|
| `--remote-url` | (required) | Git remote URL (`ssh://user@host/path` or `user@host:path`) |
| `--branch` | `main` | Branch to serve |
| `--port` | `8080` | WebDAV server port |
| `--bind` | `127.0.0.1` | Bind address |
| `--ssh-key` | — | Path to SSH private key |
| `--author-name` | `$GIT_AUTHOR_NAME` or `$USER` | Git author name for commits |
| `--author-email` | `$GIT_AUTHOR_EMAIL` | Git author email for commits |

### Authentication

SSH authentication is tried in this order:

1. **`--ssh-key`** flag, if provided
2. **SSH agent** via `$SSH_AUTH_SOCK`
3. **Default key files**: `~/.ssh/id_ed25519`, `id_rsa`, `id_ecdsa`, `id_ecdsa_sk`, `id_ed25519_sk`, `id_dsa`

## How it works

```
HTTP request  →  hyper  →  handler.rs  →  GitRepo (in-memory)
                                              │
                                              ├─ russh SSH ──→ remote Git server
                                              │                  (fetch objects)
                                              │
                                              └─ gix-objects ──→ tree/index/packfile
                                                                 (in-memory only)
```

1. On startup, davgit connects to the remote via SSH, runs `git-upload-pack`, and downloads the branch's packfile
2. All Git objects (trees, blobs, commits) are parsed and cached in memory
3. HTTP requests read/write directly from the cached state
4. Writes create a new commit and push it back via `git-receive-pack` over a fresh SSH connection

## Performance

| Operation | Time |
|---|---|
| Initial fetch | ~1.2s |
| Write cycle (fetch for write + push) | ~3.8s |

Bottleneck is remote Git server processing time, not SSH. Connection reuse (single SSH session for both upload-pack and receive-pack) would cut write time further.

## Project status

**Experimental.** The WebDAV implementation passes the basic, HTTP, and most property/copy litmus tests, but lacks lock support (LOCK/UNLOCK return 405) and XML body parsing for PROPFIND requests.

| Suite | Pass / Total |
|---|---|
| basic | 16/16 |
| http | 4/4 |
| props | 9/14 (5 fail: 2 PROPFIND body validation, 3 PROPPATCH 501) |
| copymove | 10/13 (3 fail: copy_coll overwrite, copy_shallow, move_coll) |
| locks | 0/6 (all return 405) |

## Build

Requires Rust 2024 edition. Build with Cargo as usual:

```bash
cargo build --release
```

### Dependencies

- **SSH transport**: `russh` — pure-Rust SSH client library
- **Git objects**: `gix-object`, `gix-hash`, `gix-actor`
- **HTTP server**: `hyper`, `hyper-util`, `http-body-util`
- **Packfile**: `sha1`, `flate2`, `smallvec`
- **CLI**: `clap`

## License

MIT
