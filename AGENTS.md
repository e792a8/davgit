# AGENTS.md — davgit project context

## Project goal
Pure-Rust in-memory Git WebDAV server: serve and modify remote Git repos over WebDAV without any local `.git` storage, temp dirs, or external `git`/`ssh` binaries. Uses `russh` for pure-Rust SSH transport and gix-plumbing for object handling.

## Authentication
- Agent auth (`$SSH_AUTH_SOCK`) via `AgentClient::connect_env()` + `authenticate_publickey_with`
- Default key files (`~/.ssh/id_ed25519`, `id_rsa`, `id_ecdsa`, `id_ecdsa_sk`, `id_ed25519_sk`, `id_dsa`)
- `--ssh-key <path>` flag for explicit key
- Both `ssh://user@host/path` and SCP-style `user@host:path` URL formats supported

## How to run
```bash
RUST_LOG=info cargo run --release -- \
  --remote-url 'git@example.com:user/repo.git' \
  --branch main \
  --port 18080
```

## Architecture
```
main.rs:          TcpListener → hyper service_fn → handler::handle_request
handler.rs:       handle_request → match method → direct GitRepo calls
git/repo.rs:      GitRepo — async methods: objects + index + dirs cache
git/transport.rs: russh-based async SSH fetch/push (no subprocess)
git/packfile.rs:  packfile parse/build, delta resolve
git/objects.rs:   tree walk/index build, commit build
```

### handler.rs method dispatch
| Method | Handler | Status |
|---|---|---|
| GET | `handle_get` | Returns file content with ETag |
| HEAD | `handle_head` | Same as GET, no body |
| PUT | `handle_put` | Check parent exists, write & push |
| DELETE | `handle_delete` | Remove file/dir, return 404 if not found |
| MKCOL | `handle_mkcol` | Reject body, create empty dir |
| COPY | `handle_copy_move(false)` | File/subtree copy with parent check |
| MOVE | `handle_copy_move(true)` | Copy + delete source |
| OPTIONS | `handle_options` | DAV capabilities |
| PROPFIND | `handle_propfind` | XML multistatus (inline string fmt, no XML deps) |
| PROPPATCH | → 501 | Not implemented |
| LOCK | → 405 | Not implemented |
| UNLOCK | → 405 | Not implemented |

## Key decisions
- **`russh` over alternatives**: only mature pure-Rust SSH library. Others: `ssh2` (C bindings), `ssh-rs` (unmaintained), `RustCrypto/SSH` (WIP).
- **`Channel::into_stream()` + `tokio::io::split()`** over `Channel::split()` — avoids internal `ChannelReadHalf`/`ChannelWriteHalf` types; uses `BufReader<ReadHalf>` for pkt-line parsing.
- **No `dav-server` crate**: saves ~20 transitive deps, ~5k lines, 2 `dyn` dispatches per request. Custom handler is ~480 lines.
- **No XML parsing dep**: PROPFIND response built with `format!` + `xml_escape`. Request body ignored (causes 2 litmus `propfind_invalid` failures but avoids `quick-xml`).
- **`std::sync::Mutex`** retained in `repo.rs` — locks scoped to not cross `.await` boundaries.
- **`--author-name`/`--author-email`** CLI flags with env var fallback (`GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`); no default identity.

## Performance
- Initial fetch: **~1.2s** (was ~10-12s with ssh subprocess)
- Write cycle (fetch + push): **~3.8s** (was ~20s)
- Bottleneck is remote git server processing time, not SSH
- Connection reuse (single SSH session for fetch+push) would save ~1 more SSH handshake per write

## Litmus WebDAV test results
| Suite | Pass | Fail | Notes |
|---|---|---|---|
| basic | 16/16 | 0 | |
| http | 4/4 | 0 | |
| props | 9/14 | 5 | 2 PROPFIND body validation (no XML parser), 3 PROPPATCH 501 |
| copymove | 10/13 | 3 | copy_coll overwrite (404), copy_shallow cascading, move_coll (404) |
| locks | 0 | all | LOCK/UNLOCK return 405 |

## Litmus integration tests
- **Submodule**: `tests/litmus/` → `github.com/notroj/litmus` (includes neon submodule)
- **Auto-build**: `build_litmus()` runs `autogen.sh → ./configure → make` only when Makefile is missing
- **Once per run**: `std::sync::Once` ensures C build happens exactly once
- **Run**:
  ```bash
  git submodule update --init --recursive
  REMOTE_URL='git@example.com:user/repo.git' cargo test --features litmus-tests --release -- --no-capture
  ```
- **System deps**: `autoconf`, `automake`, `libtool`, `gcc`, `make`, `libneon-dev`

## Critical context
- **`russh` SSH client flow**: `client::connect(addr, config, handler)` → `authenticate_publickey` or `authenticate_publickey_with(agent)` → `channel_open_session()` → `exec("git-upload-pack /path")` → `channel.into_stream()` → `tokio::io::split()` → async read/write for git pkt-line protocol.
- **Agent auth**: `AgentClient::connect_env()` reads `$SSH_AUTH_SOCK`. `request_identities()` → `Vec<AgentIdentity>`. `authenticate_publickey_with(user, identity.public_key().into_owned(), None, &mut agent)`.
- **Default key auth**: `load_secret_key(&path, None)` → `PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash)` → `authenticate_publickey(user, key_with_hash)`.
- **Push report**: server responds with pkt-lines (`unpack ok\n`, `ok <ref>\n`, `ng <ref> <msg>\n`, `0000`). Read via async `read_pkt_line`.
- **`is_directory` check**: `k != path && k.starts_with(path)` (bare `starts_with` matches the path itself).
- **Packfile dedup**: `build_change_commit` uses `HashSet<ObjectId>` to avoid duplicate blob OIDs when same content appears at multiple paths.
- **Empty dirs**: tracked in-memory via `GitRepo.dirs: HashSet<PathBuf>`; not persisted in git (lost on restart).
- **Protocol**: `want <oid> [caps]\n` → `0000` → `done\n` (pkt-line `0009done\n`).
- **OFS_DELTA offset encoding**: big-endian varint with `+1` per continuation byte before shift.
