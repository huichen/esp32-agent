# Storage and file-format guarantees

## Backends and acknowledgements

`SplitStore` sends `transcript`, `transcript.*`, `ws` and `ws.*` to LittleFS on
`storage`. Other keys go to NVS. Workspace paths are not physical filenames: the
index `ws.index` maps each path to a short `ws.<12 hex digits>` key. The workspace
still occupies RAM; a filesystem partition is not additional application heap.

`QueuedStore::set` acknowledges a **queued request**, not a completed flash write.
The writer drains every 100 ms on an internal-RAM stack. Inspect
`/api/status.store.pending`, `failed` and `last_failure` to distinguish accepted
requests from durable writes. A streamed write executes synchronously on the
calling thread and therefore requires an internal-RAM stack too.

The simulator uses one JSON object on disk. It serializes through a buffered writer,
syncs the temporary file and then renames it. A failed save restores the previous
in-memory value and reports an I/O error; a failed removal does not appear successful.
This is not a claim of power-loss durability on every host filesystem: directory-entry
persistence depends on the host OS and filesystem.

## LittleFS replacement and recovery

A value is written to `<key>.tmp`, synced and closed, then published with one rename
over `<key>`. A failed write or rename does not truncate the old value. A replacement
still needs space for the old and temporary files at the same time.

The sequential-read cache is closed before replacing or removing that key. This is
both freshness and an operation precondition: the ESP LittleFS VFS refuses rename
when either source or destination is open, and refuses unlink of an open file.
Physical keys are short single names; path separators, control characters, `.`/`..`
and the reserved `.tmp`/`.old` suffixes are rejected.

Startup removes unpublished `.tmp` files. Backups from the older two-rename layout
are handled separately: if `<key>` exists, it wins and `<key>.old` is discarded; if
`<key>` is missing, the backup is restored. A metadata error is not treated as
absence. Failed recovery is logged, not counted as successful.

**The mount policy still allows formatting any volume that fails to mount.** This
covers an empty partition and migration from SPIFFS, but also an unreadable LittleFS
volume. It is not a nondestructive corruption-recovery policy. Back up important
workspace contents before changing filesystem formats.

### What the dependency source establishes

The pinned component is `joltwallet/littlefs` **1.23.1**, with ESP-IDF **5.5.2**:

- `src/esp_littlefs.c`, `vfs_littlefs_mkdir`: calls `lfs_mkdir`; directories exist.
  The flat key layout is an application choice, not a LittleFS limitation.
- `vfs_littlefs_rename`: rejects open source/destination paths, then calls `lfs_rename`.
  `src/littlefs/lfs.h` documents replacing an existing destination of matching type.
- `vfs_littlefs_fsync`: calls the component's file-sync helper.
- `get_total_and_used_bytes`: total is the mounted filesystem's block count times
  configured block size; used is `lfs_fs_size` times block size, clamped to total.
  `StoreSpace` computes free as total minus used.
  Metadata and copy-on-write requirements mean free bytes are not a guaranteed file size.

The old 38-second format, conflicting-name rename error, 11,498,561-byte capacity
formula and ENOSPC-at-2.1-MB readings were **SPIFFS** observations. They do not measure
LittleFS performance or capacity.

These dependency facts and host regression tests do not verify flash power-cut
behavior. **Firmware not built/verified for these changes.** Board checks still needed:
read then replace/remove the same key; interrupt a save before/after rename; recover
legacy `.old` with and without a published value; verify ENOSPC preserves the old value.

## NVS failed replacements

`esp-idf-svc` **0.53.0**, `src/nvs.rs::set_str`, calls `nvs_erase_key` before
`nvs_set_str`. That loses the previous string if the replacement is refused. The
firmware instead validates the string and invokes `nvs_set_str` followed by
`nvs_commit` directly. ESP-IDF's `nvs_storage.cpp::Storage::writeItem` writes the new
entry before deleting the old one. This preserves the old value on a no-space refusal;
it does not make every possible flash or commit error a rollback guarantee.

## Bounded readers

- Chunked `ws1` blobs reject oversized header totals before allocation, limit checksum
  counts, and refuse chunks that exceed the declared total. UTF-8 splitting can require
  more chunks than raw byte division: writes check that actual chunk count before touching
  storage. Checksums detect torn multi-key writes; they do not preserve the preceding blob.
- ZIP reading permits stored/deflated, non-encrypted, single-disk ZIP32 entries. Payloads
  total at most **3,072 bytes**, with at most **128 entries** and **3,072 bytes of names**.
  Directory/local-header bounds, relative names, declared output lengths and CRCs are checked.
  ZIP64 and non-UTF-8 names are not supported.
- Raw inflate has a **1 MiB** default output cap; ZIP passes its smaller remaining budget.
  Literal, match and stored-block paths check space **before** appending. Oversubscribed
  Huffman tables and overflowing code-length repeats are refused.
- `/MEMORY.md` injection, including its truncation marker, is at most **2,048 bytes** and
  takes at most **24 pointer lines**. A single oversized pointer cannot bypass the byte cap.
