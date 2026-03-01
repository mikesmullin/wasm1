# PRD: TCOW Filesystem Host Functions for Wasmtime Agent

**Version** — 1.0
**Date** — February 2026
**Status** — Draft

---

## 1. Overview

This document describes the integration of a virtual copy-on-write (CoW) filesystem into the existing Wasmtime-hosted AI agent. Rather than granting the Wasm guest ambient WASI filesystem authority, all filesystem operations are mediated through a small set of explicit **host functions** that read and write to a single `.tcow` file on the host. The on-disk format is described in depth in [TCOW.md](TCOW.md).

### Goals

- Allow the Wasm guest agent to persist and retrieve files across tool calls within a single `.tcow` filesystem image.
- Never grant the guest direct WASI FS/network authority (consistent with existing security invariants).
- Keep the new host surface small, explicit, and auditable.
- Support snapshot semantics so each agent run can optionally start from a clean layer.

### Non-Goals (v1)

- WASI `wasi:filesystem` component model bindings — we use raw imported C functions, consistent with the existing approach.
- Full POSIX compliance (no `seek`, `mmap`, `fcntl`, `stat` st_mode permission bits).
- Concurrent multi-process access to a single `.tcow` file.
- Auto-compaction — the CLI tool handles compaction externally (see [FS_CLI.md](FS_CLI.md)).

---

## 2. Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Host Process (Rust / Wasmtime)                          │
│                                                          │
│  HostState                                               │
│  ├── tcow_fs: TcowFs          ← owns .tcow file handle   │
│  │   ├── layers: Vec<Layer>   ← parsed index at startup  │
│  │   ├── writable: Layer      ← in-memory write buffer   │
│  │   └── path: PathBuf        ← e.g. agent.tcow          │
│  ├── wasi: WasiCtx                                       │
│  ├── client: reqwest::Client                             │
│  └── ...                                                 │
│                                                          │
│  Linker                                                  │
│  ├── host::fs_read   ─────────────────────────────┐      │
│  ├── host::fs_write  ──────────────────────────── │ ──┐  │
│  ├── host::fs_delete ─────────────────────────────│   │  │
│  ├── host::fs_list   ─────────────────────────────│   │  │
│  └── host::fs_stat   ─────────────────────────────│   │  │
│                                                   ▼   ▼  │
│                                             TcowFs API   │
│                                                   │      │
└───────────────────────────────────────────────────│──────┘
                                                    │
                                              agent.tcow
                                          (on host filesystem)
```

The guest calls host functions using the same pattern already established for `grok_chat` and `js_exec`: pointer + length pairs for input, a caller-supplied output buffer with a capacity, and a return value conveying the written byte count (or a negative error sentinel).

---

## 3. HostState Changes

Add `TcowFs` to `HostState` in `src/main.rs`:

```rust
use crate::tcow::{TcowFs, TcowError};

struct HostState {
    // --- existing fields ---
    prompt: String,
    final_answer: Option<String>,
    api_key: String,
    model: String,
    client: Client,
    wasi: WasiCtx,

    // --- new field ---
    /// Virtual copy-on-write filesystem backed by a single `.tcow` file.
    fs: TcowFs,
}
```

`TcowFs` is opened/created at startup before `Linker` construction:

```rust
let tcow_path = env::var("TCOW_PATH").unwrap_or_else(|_| "agent.tcow".into());
let fs = TcowFs::open_or_create(&tcow_path)?;
```

---

## 4. New Host Functions

All five functions are registered in the `"host"` import module, matching the convention used by existing host functions.

### Naming Convention

Guest-side signatures use `i32` exclusively — Wasm's lowest-common-denominator integer for pointer/length/boolean values. Lengths are unsigned-interpreted `i32`s (cast to `u32` on the host).

### 4.1 `fs_read`

Read the contents of a file at `path` from the virtual filesystem.

**Host signature (Rust):**

```rust
fn fs_read(
    mut caller: Caller<'_, HostState>,
    path_ptr: i32, path_len: i32,   // UTF-8 path in guest memory
    out_ptr:  i32, out_cap: i32,    // output buffer in guest memory
) -> i32  // bytes written, or negative error code
```

**Guest import (Rust):**

```rust
#[link(wasm_import_module = "host")]
unsafe extern "C" {
    fn fs_read(
        path_ptr: i32, path_len: i32,
        out_ptr:  i32, out_cap:  i32,
    ) -> i32;
}
```

**Semantics:**
- Resolves `path` against the union view of all layers (top-to-bottom).
- Copies file bytes into the guest output buffer up to `out_cap` bytes.
- Returns the number of bytes actually written, or a negative sentinel:
  - `-1` — file not found
  - `-2` — output buffer too small (caller should retry with a larger buffer)
  - `-3` — path encoding error
  - `-4` — I/O or TCOW format error

---

### 4.2 `fs_write`

Create or replace a file at `path` in the **current writable layer**.

**Host signature (Rust):**

```rust
fn fs_write(
    mut caller: Caller<'_, HostState>,
    path_ptr:    i32, path_len:    i32,   // UTF-8 path
    content_ptr: i32, content_len: i32,   // raw bytes to write
) -> i32  // 0 on success, negative error code on failure
```

**Guest import (Rust):**

```rust
fn fs_write(
    path_ptr:    i32, path_len:    i32,
    content_ptr: i32, content_len: i32,
) -> i32;
```

**Semantics:**
- If the file already exists in a lower (read-only) layer, a **copy-up** is performed: the original bytes are read and a new entry is written into the writable layer before being replaced with the new content. This preserves the immutability of lower layers.
- If the file already exists in the writable layer, it is replaced in-place (the old writable-layer entry is shadowed by a new one appended to the in-memory write buffer; the old bytes become unreachable until compaction).
- The write is buffered in memory (`TcowFs::writable`). It is flushed to disk on `TcowFs::flush()`, which is called:
  - On graceful agent exit.
  - When `fs_write` accumulates more than a configurable threshold (default: 8 MiB).
- Returns `0` on success, or:
  - `-3` — path encoding error
  - `-4` — I/O or format error

---

### 4.3 `fs_delete`

Mark a file as deleted in the current writable layer using a **whiteout entry**.

**Host signature (Rust):**

```rust
fn fs_delete(
    mut caller: Caller<'_, HostState>,
    path_ptr: i32, path_len: i32,
) -> i32  // 0 on success, -1 not found, -3 encoding, -4 I/O
```

**Guest import (Rust):**

```rust
fn fs_delete(path_ptr: i32, path_len: i32) -> i32;
```

**Semantics:**
- Appends a whiteout tar entry (`/.wh.<basename>`) to the in-memory writable layer.
- After a whiteout is written, `fs_read`, `fs_stat`, and `fs_list` treat the file as non-existent even if it appears in a lower layer.
- Deleting a file that does not exist in any layer returns `-1`.
- Deleting a directory path deletes the directory whiteout marker; individual children require their own whiteout entries (opaque whiteout `.wh..wh..opq` may be added in a future version).

---

### 4.4 `fs_list`

List the names of all visible entries under a directory path.

**Host signature (Rust):**

```rust
fn fs_list(
    mut caller: Caller<'_, HostState>,
    dir_ptr: i32, dir_len: i32,   // directory path (e.g. b"/data/")
    out_ptr: i32, out_cap: i32,   // output buffer
) -> i32  // bytes written, or negative error code
```

**Guest import (Rust):**

```rust
fn fs_list(
    dir_ptr: i32, dir_len: i32,
    out_ptr: i32, out_cap: i32,
) -> i32;
```

**Semantics:**
- Returns a **newline-delimited** list of entry names (no leading path, no trailing slash for files; trailing `/` for directories) written into the output buffer.
- The union logic is applied: an entry appearing in a higher layer shadows a same-named entry in a lower layer; whiteout entries suppress the corresponding name.
- Listing a path that does not correspond to any directory in any layer returns `-1`.
- If the result does not fit in `out_cap`, returns `-2`; the caller should retry with a larger buffer.

---

### 4.5 `fs_stat`

Return metadata for a file without reading its contents.

**Host signature (Rust):**

```rust
fn fs_stat(
    mut caller: Caller<'_, HostState>,
    path_ptr: i32, path_len: i32,
    out_ptr:  i32, out_cap:  i32,   // receives UTF-8 JSON metadata
) -> i32  // bytes written, or negative error code
```

**Guest import (Rust):**

```rust
fn fs_stat(
    path_ptr: i32, path_len: i32,
    out_ptr:  i32, out_cap:  i32,
) -> i32;
```

**Semantics:**
- Writes a small JSON object into the output buffer:

```json
{
  "size": 4096,
  "mtime": "2026-02-28T14:32:00Z",
  "layer": 2,
  "whiteout": false
}
```

| Field | Type | Description |
|---|---|---|
| `size` | `u64` | File size in bytes |
| `mtime` | RFC 3339 string | Modification time from tar header |
| `layer` | `u32` | 0-indexed layer where this version of the file lives |
| `whiteout` | `bool` | `true` if the topmost record for this path is a whiteout |

- Returns `-1` if the path does not exist in any layer (including whiteout-deleted).

---

## 5. Linker Registration

All five functions are registered in `src/main.rs` inside the existing linker setup block:

```rust
// src/main.rs — inside fn setup_linker(...)

linker.func_wrap("host", "fs_read", |mut caller: Caller<'_, HostState>,
    path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32| -> i32 {
    let path = read_guest_str(&mut caller, path_ptr, path_len)?;
    match caller.data().fs.read(&path) {
        Err(TcowError::NotFound) => return -1,
        Err(_) => return -4,
        Ok(bytes) => {
            if bytes.len() > out_cap as usize { return -2; }
            write_guest_bytes(&mut caller, out_ptr, &bytes);
            bytes.len() as i32
        }
    }
})?;

linker.func_wrap("host", "fs_write", |mut caller: Caller<'_, HostState>,
    path_ptr: i32, path_len: i32, content_ptr: i32, content_len: i32| -> i32 {
    let path    = read_guest_str(&mut caller, path_ptr, path_len)?;
    let content = read_guest_bytes(&mut caller, content_ptr, content_len);
    match caller.data_mut().fs.write(&path, content) {
        Ok(())  => 0,
        Err(_)  => -4,
    }
})?;

// ... fs_delete, fs_list, fs_stat registered similarly
```

Helper functions `read_guest_str`, `read_guest_bytes`, and `write_guest_bytes` extract/inject data from the linear memory of the caller, following the same helper pattern already used by `grok_chat`.

---

## 6. Module Layout

The TCOW filesystem logic lives in its own crate module:

```
src/
  main.rs          ← HostState gains `fs: TcowFs`; linker gains 5 new funcs
  tcow/
    mod.rs         ← pub use; re-exports TcowFs, TcowError
    fs.rs          ← TcowFs struct, open_or_create, read, write, delete, list, stat, flush
    layer.rs       ← Layer struct, tar-stream parser, union-view logic
    index.rs       ← CBOR trailer: TcowIndex, write_trailer, read_trailer
    whiteout.rs    ← whiteout path <-> normal path conversion helpers
    error.rs       ← TcowError enum
```

The `tcow` module is also compiled as a library target (`src/lib.rs` re-exporting `tcow`) so the standalone CLI (`tcow-cli`) can depend on it without duplicating code. See [FS_CLI.md](FS_CLI.md).

---

## 7. Cargo.toml Changes

```toml
[dependencies]
# existing deps...
tar       = "0.4"
serde_cbor = "0.11"    # or ciborium = "0.2" (maintained fork)
chrono    = { version = "0.4", features = ["serde"] }

[[bin]]
name = "tcow"
path = "src/bin/tcow.rs"   # standalone CLI entry point
```

---

## 8. Guest Changes

Add the five extern imports to `guest/src/lib.rs`:

```rust
#[link(wasm_import_module = "host")]
unsafe extern "C" {
    // existing
    fn get_prompt(out_ptr: i32, out_cap: i32) -> i32;
    fn host_log(ptr: i32, len: i32);
    fn emit_final(ptr: i32, len: i32);
    fn grok_chat(req_ptr: i32, req_len: i32, out_ptr: i32, out_cap: i32) -> i32;

    // new filesystem
    fn fs_read  (path_ptr: i32, path_len: i32, out_ptr:  i32, out_cap:  i32) -> i32;
    fn fs_write (path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn fs_delete(path_ptr: i32, path_len: i32) -> i32;
    fn fs_list  (dir_ptr:  i32, dir_len:  i32, out_ptr:  i32, out_cap:  i32) -> i32;
    fn fs_stat  (path_ptr: i32, path_len: i32, out_ptr:  i32, out_cap:  i32) -> i32;
}
```

Safe wrapper helpers in the guest:

```rust
fn vfs_write(path: &str, content: &[u8]) -> Result<(), i32> {
    let rc = unsafe {
        fs_write(
            path.as_ptr() as i32, path.len() as i32,
            content.as_ptr() as i32, content.len() as i32,
        )
    };
    if rc == 0 { Ok(()) } else { Err(rc) }
}

fn vfs_read(path: &str, buf: &mut Vec<u8>) -> Result<usize, i32> {
    buf.resize(256 * 1024, 0); // initial 256 KiB attempt
    let rc = unsafe {
        fs_read(
            path.as_ptr() as i32, path.len() as i32,
            buf.as_mut_ptr() as i32, buf.len() as i32,
        )
    };
    if rc >= 0 { Ok(rc as usize) }
    else if rc == -2 {
        buf.resize(8 * 1024 * 1024, 0); // retry with 8 MiB
        let rc2 = unsafe {
            fs_read(
                path.as_ptr() as i32, path.len() as i32,
                buf.as_mut_ptr() as i32, buf.len() as i32,
            )
        };
        if rc2 >= 0 { Ok(rc2 as usize) } else { Err(rc2) }
    } else { Err(rc) }
}
```

---

## 9. Error Codes Summary

| Code | Meaning |
|------|---------|
| `0`  | Success (write/delete functions) |
| `≥1` | Bytes written (read/list/stat functions) |
| `-1` | Not found |
| `-2` | Output buffer too small |
| `-3` | Invalid UTF-8 or malformed path |
| `-4` | I/O or TCOW format error |

---

## 10. Startup / Shutdown Flow

```
main()
  ├── TcowFs::open_or_create("agent.tcow")
  │     ├── if file exists: parse CBOR trailer → build layer index
  │     └── if not: create file with empty base layer
  │
  ├── register host functions (including fs_*)
  ├── instantiate Wasm module
  ├── call guest run()
  │     └── guest calls fs_* as needed
  │
  └── TcowFs::flush()        ← appends dirty writable layer + updated trailer
        └── file is now a valid .tcow with one additional delta layer
```

---

## 11. Testing Plan

| Scenario | Verification |
|---|---|
| Write a file, read it back | Bytes match |
| Write in layer 0, read after re-open (new layer) | Read falls through to layer 0 |
| Overwrite a lower-layer file | Copy-up occurs; lower layer unchanged |
| Delete a lower-layer file | Whiteout inserted; file not returned by read/list |
| `fs_list` on a root dir | All visible entries returned, whiteouts excluded |
| `fs_stat` on existing / missing file | Correct JSON / -1 |
| Buffer-too-small retry | Second call with larger cap succeeds |
| `tcow info agent.tcow` (CLI) | Layers and file count match what was written |

---

## 12. Future Work

- `fs_snapshot` host function to seal the current layer and start a new writable layer mid-run.
- Opaque whiteout (`.wh..wh..opq`) to delete entire directory trees atomically.
- Configurable flush threshold and async flush on a background thread.
- Compression option per layer (gzip) for long-lived agents with large file stores.
- WASI `wasi:filesystem` adapter shim so existing WASI-using guests work without source changes.
