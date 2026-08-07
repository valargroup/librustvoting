//! [`KvShardStore`] — a [`ShardStore`] implementation backed by Go's Cosmos KV
//! store via C function pointer callbacks.
//!
//! # Design
//!
//! Instead of maintaining an in-process copy of all shard data,
//! `KvShardStore` forwards every [`ShardStore`] read and write directly to
//! the Cosmos KV store through a set of C callbacks registered at creation
//! time. Go registers `//export` functions that dispatch to the current
//! block's `store.KVStore` through a stable proxy pointer.
//!
//! This gives `ShardTree` true lazy loading: on a cold start only the data
//! that is actually accessed (the frontier shard + cap + checkpoints) is read.
//! No explicit restore loop, no O(n) blob loading, no shard geometry in Go.
//!
//! # KV key schema (matches keys.go)
//!
//! | Prefix    | Key                              | Value           |
//! |-----------|----------------------------------|-----------------|
//! | `0x0F`    | `0x0F \|\| u64 BE shard_index`   | shard blob      |
//! | `0x10`    | `0x10`                           | cap blob        |
//! | `0x11`    | `0x11 \|\| u32 BE checkpoint_id` | checkpoint blob |
//! | `0x12`    | `0x12 \|\| u32 BE checkpoint_id` | retained marker |
//!
//! # Buffer ownership
//!
//! `get` returns a C-malloc'd buffer that Rust frees with the provided
//! `free_buf` callback after copying the value. All write callbacks receive
//! a Rust-owned slice (pointer + length); they must copy the data if they
//! need it to outlive the call.
//!
//! # Iterator protocol
//!
//! `iter_create(ctx, prefix, prefix_len, reverse)` returns an opaque handle
//! (a `cgo.Handle` on the Go side). `iter_next` advances and writes
//! C-malloc'd key + value; Rust frees each pair with `free_buf` before the
//! next call. `iter_free` closes and drops the iterator. `iter_next` returns
//! 0 on a valid entry, 1 when exhausted, -1 on error.

use std::collections::BTreeSet;
use std::fmt;
use std::os::raw::c_void;

use incrementalmerkletree::{Address, Level};
use shardtree::{
    store::{Checkpoint, ShardStore},
    LocatedPrunableTree, LocatedTree, PrunableTree, Tree,
};

use crate::hash::{MerkleHashVote, SHARD_HEIGHT};
use crate::serde::{read_checkpoint, read_shard_vote, write_checkpoint, write_shard_vote};

// ---------------------------------------------------------------------------
// KvError
// ---------------------------------------------------------------------------

/// Error type for [`KvShardStore`] operations.
///
/// Replaces `Infallible` so that KV callback failures are visible to callers
/// rather than being silently swallowed. The three variants cover all
/// observable failure modes:
///
/// - `IoError`: a KV callback returned a non-zero error code (disk full,
///   store closed, etc.).
/// - `Deserialization`: a blob retrieved from KV failed to decode.
/// - `Serialization`: a shard or cap could not be encoded before writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvError {
    /// A KV callback returned an error code (set, delete, or iterator failure).
    IoError,
    /// Shard or checkpoint data retrieved from KV could not be decoded.
    Deserialization,
    /// Shard or cap data could not be serialized before writing.
    Serialization,
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KvError::IoError => write!(f, "KV callback returned an error"),
            KvError::Deserialization => write!(f, "failed to deserialize KV data"),
            KvError::Serialization => write!(f, "failed to serialize data for KV"),
        }
    }
}

impl std::error::Error for KvError {}

// ---------------------------------------------------------------------------
// KV key constants (must match keys.go 0x0F / 0x10 / 0x11 / 0x12)
// ---------------------------------------------------------------------------

const SHARD_PREFIX: u8 = 0x0F;
const CAP_KEY: u8 = 0x10;
const CHECKPOINT_PREFIX: u8 = 0x11;
const RETAINED_CHECKPOINT_PREFIX: u8 = 0x12;

fn shard_key(index: u64) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = SHARD_PREFIX;
    k[1..].copy_from_slice(&index.to_be_bytes());
    k
}

fn cap_key() -> [u8; 1] {
    [CAP_KEY]
}

fn checkpoint_key(id: u32) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[0] = CHECKPOINT_PREFIX;
    k[1..].copy_from_slice(&id.to_be_bytes());
    k
}

fn retained_checkpoint_key(id: u32) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[0] = RETAINED_CHECKPOINT_PREFIX;
    k[1..].copy_from_slice(&id.to_be_bytes());
    k
}

fn parse_shard_key(key: &[u8]) -> Option<u64> {
    let index: [u8; 8] = key.strip_prefix(&[SHARD_PREFIX])?.try_into().ok()?;
    Some(u64::from_be_bytes(index))
}

fn parse_checkpoint_key(key: &[u8]) -> Option<u32> {
    let id: [u8; 4] = key.strip_prefix(&[CHECKPOINT_PREFIX])?.try_into().ok()?;
    Some(u32::from_be_bytes(id))
}

fn parse_retained_checkpoint_key(key: &[u8]) -> Option<u32> {
    let id: [u8; 4] = key
        .strip_prefix(&[RETAINED_CHECKPOINT_PREFIX])?
        .try_into()
        .ok()?;
    Some(u32::from_be_bytes(id))
}

// ---------------------------------------------------------------------------
// Callback function pointer types
// ---------------------------------------------------------------------------

/// Retrieve a value from the KV store.
///
/// On success (key found) writes a C-malloc'd buffer to `*out_val` and its
/// length to `*out_val_len`, then returns 0.
/// Returns 1 if the key was not found (out pointers are unchanged).
/// Returns -1 on error.
pub type KvGetFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    key: *const u8,
    key_len: usize,
    out_val: *mut *mut u8,
    out_val_len: *mut usize,
) -> i32;

/// Write a key-value pair. Returns 0 on success, -1 on error.
pub type KvSetFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    key: *const u8,
    key_len: usize,
    val: *const u8,
    val_len: usize,
) -> i32;

/// Delete a key. Returns 0 on success, -1 on error.
pub type KvDeleteFn = unsafe extern "C" fn(ctx: *mut c_void, key: *const u8, key_len: usize) -> i32;

/// Create an iterator over the given prefix.
///
/// `reverse` is 1 for a reverse (descending) iterator, 0 for ascending.
/// Returns an opaque iterator handle, or null on error.
pub type KvIterCreateFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    prefix: *const u8,
    prefix_len: usize,
    reverse: u8,
) -> *mut c_void;

/// Advance the iterator and return the next key-value pair as C-malloc'd
/// buffers. Caller frees with `free_buf`.
///
/// Returns 0 if a valid entry was written, 1 if exhausted, -1 on error.
pub type KvIterNextFn = unsafe extern "C" fn(
    iter: *mut c_void,
    out_key: *mut *mut u8,
    out_key_len: *mut usize,
    out_val: *mut *mut u8,
    out_val_len: *mut usize,
) -> i32;

/// Close and free an iterator handle.
pub type KvIterFreeFn = unsafe extern "C" fn(iter: *mut c_void);

/// Free a C-malloc'd buffer returned by a KV callback.
pub type KvFreeBufFn = unsafe extern "C" fn(ptr: *mut u8, len: usize);

// ---------------------------------------------------------------------------
// KvCallbacks
// ---------------------------------------------------------------------------

/// Bundle of C function pointers + context passed to [`KvShardStore`].
///
/// # Safety
/// All function pointers must remain valid for the lifetime of the
/// `KvShardStore`. The `ctx` pointer must remain stable; Go achieves this
/// via a `KvStoreProxy` whose address never changes across blocks.
#[derive(Clone, Copy)]
pub struct KvCallbacks {
    pub ctx: *mut c_void,
    pub get: KvGetFn,
    pub set: KvSetFn,
    pub delete: KvDeleteFn,
    pub iter_create: KvIterCreateFn,
    pub iter_next: KvIterNextFn,
    pub iter_free: KvIterFreeFn,
    pub free_buf: KvFreeBufFn,
}

// SAFETY: EndBlocker is single-threaded; all callbacks are called only on
// the goroutine that owns the KV store.
unsafe impl Send for KvCallbacks {}
unsafe impl Sync for KvCallbacks {}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

impl KvCallbacks {
    /// Fetch a value by key.
    ///
    /// Returns `Ok(Some(bytes))` if found, `Ok(None)` if not present, or
    /// `Err(KvError::IoError)` if the callback signalled a hard error (rc=-1).
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, KvError> {
        let mut out_ptr: *mut u8 = std::ptr::null_mut();
        let mut out_len: usize = 0;
        let rc = unsafe {
            (self.get)(
                self.ctx,
                key.as_ptr(),
                key.len(),
                &mut out_ptr,
                &mut out_len,
            )
        };
        match rc {
            0 => {
                let val = unsafe { std::slice::from_raw_parts(out_ptr, out_len).to_vec() };
                unsafe { (self.free_buf)(out_ptr, out_len) };
                Ok(Some(val))
            }
            1 => Ok(None),              // not found
            _ => Err(KvError::IoError), // rc=-1 or any other error code
        }
    }

    /// Write a key-value pair. Returns `Err(KvError::IoError)` if the
    /// callback returned a non-zero code.
    pub fn set(&self, key: &[u8], val: &[u8]) -> Result<(), KvError> {
        let rc = unsafe { (self.set)(self.ctx, key.as_ptr(), key.len(), val.as_ptr(), val.len()) };
        if rc != 0 {
            Err(KvError::IoError)
        } else {
            Ok(())
        }
    }

    /// Delete a key. Returns `Err(KvError::IoError)` if the callback failed.
    pub fn delete(&self, key: &[u8]) -> Result<(), KvError> {
        let rc = unsafe { (self.delete)(self.ctx, key.as_ptr(), key.len()) };
        if rc != 0 {
            Err(KvError::IoError)
        } else {
            Ok(())
        }
    }

    /// Create a forward or reverse iterator over the given prefix.
    fn iter(&self, prefix: &[u8], reverse: bool) -> Result<KvIter<'_>, KvError> {
        let handle =
            unsafe { (self.iter_create)(self.ctx, prefix.as_ptr(), prefix.len(), reverse as u8) };
        if handle.is_null() {
            Err(KvError::IoError)
        } else {
            Ok(KvIter { handle, cb: self })
        }
    }
}

struct KvIter<'a> {
    handle: *mut c_void,
    cb: &'a KvCallbacks,
}

type KvEntry = (Vec<u8>, Vec<u8>);

impl<'a> KvIter<'a> {
    /// Advance and return an entry, normal exhaustion, or a callback error.
    fn next(&mut self) -> Result<Option<KvEntry>, KvError> {
        let mut key_ptr: *mut u8 = std::ptr::null_mut();
        let mut key_len: usize = 0;
        let mut val_ptr: *mut u8 = std::ptr::null_mut();
        let mut val_len: usize = 0;
        let rc = unsafe {
            (self.cb.iter_next)(
                self.handle,
                &mut key_ptr,
                &mut key_len,
                &mut val_ptr,
                &mut val_len,
            )
        };
        match rc {
            0 => {
                let key = self.copy_and_free(key_ptr, key_len)?;
                let val = self.copy_and_free(val_ptr, val_len)?;
                Ok(Some((key, val)))
            }
            1 => Ok(None),
            _ => Err(KvError::IoError),
        }
    }

    fn copy_and_free(&self, ptr: *mut u8, len: usize) -> Result<Vec<u8>, KvError> {
        if ptr.is_null() {
            return if len == 0 {
                Ok(Vec::new())
            } else {
                Err(KvError::IoError)
            };
        }

        let bytes = unsafe { std::slice::from_raw_parts(ptr, len).to_vec() };
        unsafe { (self.cb.free_buf)(ptr, len) };
        Ok(bytes)
    }
}

impl<'a> Drop for KvIter<'a> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.cb.iter_free)(self.handle) };
        }
    }
}

// ---------------------------------------------------------------------------
// KvShardStore
// ---------------------------------------------------------------------------

/// A [`ShardStore`] that stores all state in the Cosmos KV store via Go
/// callbacks. Gives `ShardTree` true lazy loading: only the data it actually
/// accesses is read from KV.
pub struct KvShardStore {
    pub(crate) cb: KvCallbacks,
}

impl KvShardStore {
    pub fn new(cb: KvCallbacks) -> Self {
        Self { cb }
    }

    fn checkpoints(&self) -> Result<Vec<(u32, Checkpoint)>, KvError> {
        let prefix = [CHECKPOINT_PREFIX];
        let mut iter = self.cb.iter(&prefix, false)?;
        let mut checkpoints = Vec::new();
        while let Some((key, value)) = iter.next()? {
            let Some(id) = parse_checkpoint_key(&key) else {
                continue;
            };
            let checkpoint = read_checkpoint(&value).map_err(|_| KvError::Deserialization)?;
            checkpoints.push((id, checkpoint));
        }
        checkpoints.sort_by_key(|(id, _)| *id);
        Ok(checkpoints)
    }
}

// ---------------------------------------------------------------------------
// ShardStore implementation
// ---------------------------------------------------------------------------

impl ShardStore for KvShardStore {
    type H = MerkleHashVote;
    type CheckpointId = u32;
    type Error = KvError;

    fn get_shard(
        &self,
        shard_root: Address,
    ) -> Result<Option<LocatedPrunableTree<MerkleHashVote>>, KvError> {
        let idx = shard_root.index();
        let key = shard_key(idx);
        let Some(blob) = self.cb.get(&key)? else {
            return Ok(None);
        };
        let tree = read_shard_vote(&blob).map_err(|_| KvError::Deserialization)?;
        LocatedTree::from_parts(shard_root, tree)
            .map(Some)
            .map_err(|_| KvError::Deserialization)
    }

    fn last_shard(&self) -> Result<Option<LocatedPrunableTree<MerkleHashVote>>, KvError> {
        let prefix = [SHARD_PREFIX];
        let mut iter = self.cb.iter(&prefix, true /* reverse */)?;
        while let Some((key, value)) = iter.next()? {
            let Some(index) = parse_shard_key(&key) else {
                continue;
            };
            let address = Address::from_parts(Level::from(SHARD_HEIGHT), index);
            let tree = read_shard_vote(&value).map_err(|_| KvError::Deserialization)?;
            return LocatedTree::from_parts(address, tree)
                .map(Some)
                .map_err(|_| KvError::Deserialization);
        }
        Ok(None)
    }

    fn put_shard(&mut self, subtree: LocatedPrunableTree<MerkleHashVote>) -> Result<(), KvError> {
        let idx = subtree.root_addr().index();
        let key = shard_key(idx);
        let blob = write_shard_vote(subtree.root()).map_err(|_| KvError::Serialization)?;
        self.cb.set(&key, &blob)
    }

    fn get_shard_roots(&self) -> Result<Vec<Address>, KvError> {
        let prefix = [SHARD_PREFIX];
        let mut iter = self.cb.iter(&prefix, false)?;
        let level = Level::from(SHARD_HEIGHT);
        let mut roots = Vec::new();
        while let Some((key, _)) = iter.next()? {
            if let Some(index) = parse_shard_key(&key) {
                roots.push(Address::from_parts(level, index));
            }
        }
        Ok(roots)
    }

    fn truncate_shards(&mut self, shard_index: u64) -> Result<(), KvError> {
        let prefix = [SHARD_PREFIX];
        let mut iter = self.cb.iter(&prefix, false)?;
        let mut to_delete = Vec::new();
        while let Some((key, _)) = iter.next()? {
            if parse_shard_key(&key).is_some_and(|index| index >= shard_index) {
                to_delete.push(key);
            }
        }
        drop(iter);
        for key in to_delete {
            self.cb.delete(&key)?;
        }
        Ok(())
    }

    fn get_cap(&self) -> Result<PrunableTree<MerkleHashVote>, KvError> {
        let key = cap_key();
        let Some(blob) = self.cb.get(&key)? else {
            return Ok(Tree::empty());
        };
        read_shard_vote(&blob).map_err(|_| KvError::Deserialization)
    }

    fn put_cap(&mut self, cap: PrunableTree<MerkleHashVote>) -> Result<(), KvError> {
        let key = cap_key();
        let blob = write_shard_vote(&cap).map_err(|_| KvError::Serialization)?;
        self.cb.set(&key, &blob)
    }

    fn min_checkpoint_id(&self) -> Result<Option<u32>, KvError> {
        Ok(self.checkpoints()?.first().map(|(id, _)| *id))
    }

    fn max_checkpoint_id(&self) -> Result<Option<u32>, KvError> {
        Ok(self.checkpoints()?.last().map(|(id, _)| *id))
    }

    fn add_checkpoint(
        &mut self,
        checkpoint_id: u32,
        checkpoint: Checkpoint,
    ) -> Result<(), KvError> {
        let key = checkpoint_key(checkpoint_id);
        let blob = write_checkpoint(&checkpoint);
        self.cb.set(&key, &blob)
    }

    fn checkpoint_count(&self) -> Result<usize, KvError> {
        Ok(self.checkpoints()?.len())
    }

    fn get_checkpoint_at_depth(
        &self,
        checkpoint_depth: usize,
    ) -> Result<Option<(u32, Checkpoint)>, KvError> {
        Ok(self.checkpoints()?.into_iter().rev().nth(checkpoint_depth))
    }

    fn get_checkpoint(&self, checkpoint_id: &u32) -> Result<Option<Checkpoint>, KvError> {
        let key = checkpoint_key(*checkpoint_id);
        let Some(blob) = self.cb.get(&key)? else {
            return Ok(None);
        };
        read_checkpoint(&blob)
            .map(Some)
            .map_err(|_| KvError::Deserialization)
    }

    fn with_checkpoints<F>(&mut self, limit: usize, mut callback: F) -> Result<(), KvError>
    where
        F: FnMut(&u32, &Checkpoint) -> Result<(), KvError>,
    {
        for (id, checkpoint) in self.checkpoints()?.into_iter().take(limit) {
            callback(&id, &checkpoint)?;
        }
        Ok(())
    }

    fn for_each_checkpoint<F>(&self, limit: usize, mut callback: F) -> Result<(), KvError>
    where
        F: FnMut(&u32, &Checkpoint) -> Result<(), KvError>,
    {
        for (id, checkpoint) in self.checkpoints()?.into_iter().take(limit) {
            callback(&id, &checkpoint)?;
        }
        Ok(())
    }

    fn update_checkpoint_with<F>(&mut self, checkpoint_id: &u32, update: F) -> Result<bool, KvError>
    where
        F: Fn(&mut Checkpoint) -> Result<(), KvError>,
    {
        let key = checkpoint_key(*checkpoint_id);
        let Some(blob) = self.cb.get(&key)? else {
            return Ok(false);
        };
        let mut cp = read_checkpoint(&blob).map_err(|_| KvError::Deserialization)?;
        update(&mut cp)?;
        let new_blob = write_checkpoint(&cp);
        self.cb.set(&key, &new_blob)?;
        Ok(true)
    }

    fn remove_checkpoint(&mut self, checkpoint_id: &u32) -> Result<(), KvError> {
        let key = checkpoint_key(*checkpoint_id);
        self.cb.delete(&key)
    }

    fn add_retained_checkpoint(&mut self, checkpoint_id: u32) -> Result<(), KvError> {
        let key = retained_checkpoint_key(checkpoint_id);
        self.cb.set(&key, &[])
    }

    fn remove_retained_checkpoint(&mut self, checkpoint_id: &u32) -> Result<(), KvError> {
        let key = retained_checkpoint_key(*checkpoint_id);
        self.cb.delete(&key)
    }

    fn retained_checkpoints(&self) -> Result<BTreeSet<u32>, KvError> {
        let prefix = [RETAINED_CHECKPOINT_PREFIX];
        let mut iter = self.cb.iter(&prefix, false)?;
        let mut checkpoints = BTreeSet::new();
        while let Some((key, _)) = iter.next()? {
            if let Some(id) = parse_retained_checkpoint_key(&key) {
                checkpoints.insert(id);
            }
        }
        Ok(checkpoints)
    }

    fn truncate_checkpoints_retaining(&mut self, checkpoint_id: &u32) -> Result<(), KvError> {
        // Delete all checkpoints with id > checkpoint_id; clear marks_removed
        // on the retained checkpoint itself (matches MemoryShardStore semantics).
        let prefix = [CHECKPOINT_PREFIX];
        let mut iter = self.cb.iter(&prefix, false)?;
        let mut to_delete = Vec::new();
        while let Some((key, _)) = iter.next()? {
            if parse_checkpoint_key(&key).is_some_and(|id| id > *checkpoint_id) {
                to_delete.push(key);
            }
        }
        drop(iter);
        for key in to_delete {
            self.cb.delete(&key)?;
        }
        // Clear marks_removed on the retaining checkpoint.
        let retain_key = checkpoint_key(*checkpoint_id);
        if let Some(blob) = self.cb.get(&retain_key)? {
            let checkpoint = read_checkpoint(&blob).map_err(|_| KvError::Deserialization)?;
            let cleared = Checkpoint::from_parts(checkpoint.tree_state(), BTreeSet::new());
            self.cb.set(&retain_key, &write_checkpoint(&cleared))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ptr;

    use shardtree::{
        error::ShardTreeError,
        store::{Checkpoint, ShardStore},
        PrunableTree, Tree,
    };

    use super::*;
    use crate::{server::TreeServer, SyncableServer, TreeSyncApi};

    #[derive(Default)]
    struct TestKv {
        entries: BTreeMap<Vec<u8>, Vec<u8>>,
        fail_get: bool,
        fail_iter_create: bool,
        fail_iter_at: Option<usize>,
    }

    struct TestIter {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        index: usize,
        fail_at: Option<usize>,
    }

    unsafe fn write_buffer(bytes: &[u8], out_ptr: *mut *mut u8, out_len: *mut usize) -> bool {
        *out_len = bytes.len();
        if bytes.is_empty() {
            *out_ptr = ptr::null_mut();
            return true;
        }

        let allocated = libc::malloc(bytes.len()).cast::<u8>();
        if allocated.is_null() {
            return false;
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), allocated, bytes.len());
        *out_ptr = allocated;
        true
    }

    unsafe extern "C" fn test_get(
        ctx: *mut c_void,
        key: *const u8,
        key_len: usize,
        out_val: *mut *mut u8,
        out_val_len: *mut usize,
    ) -> i32 {
        let store = &mut *ctx.cast::<TestKv>();
        if store.fail_get {
            return -1;
        }
        let key = std::slice::from_raw_parts(key, key_len);
        match store.entries.get(key) {
            Some(value) if write_buffer(value, out_val, out_val_len) => 0,
            Some(_) => -1,
            None => 1,
        }
    }

    unsafe extern "C" fn test_set(
        ctx: *mut c_void,
        key: *const u8,
        key_len: usize,
        value: *const u8,
        value_len: usize,
    ) -> i32 {
        let store = &mut *ctx.cast::<TestKv>();
        let key = std::slice::from_raw_parts(key, key_len).to_vec();
        let value = std::slice::from_raw_parts(value, value_len).to_vec();
        store.entries.insert(key, value);
        0
    }

    unsafe extern "C" fn test_delete(ctx: *mut c_void, key: *const u8, key_len: usize) -> i32 {
        let store = &mut *ctx.cast::<TestKv>();
        let key = std::slice::from_raw_parts(key, key_len);
        store.entries.remove(key);
        0
    }

    unsafe extern "C" fn test_iter_create(
        ctx: *mut c_void,
        prefix: *const u8,
        prefix_len: usize,
        reverse: u8,
    ) -> *mut c_void {
        let store = &mut *ctx.cast::<TestKv>();
        if store.fail_iter_create {
            return ptr::null_mut();
        }
        let prefix = std::slice::from_raw_parts(prefix, prefix_len);
        let mut entries = store
            .entries
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        if reverse != 0 {
            entries.reverse();
        }
        Box::into_raw(Box::new(TestIter {
            entries,
            index: 0,
            fail_at: store.fail_iter_at,
        }))
        .cast()
    }

    unsafe extern "C" fn test_iter_next(
        iter: *mut c_void,
        out_key: *mut *mut u8,
        out_key_len: *mut usize,
        out_val: *mut *mut u8,
        out_val_len: *mut usize,
    ) -> i32 {
        let iter = &mut *iter.cast::<TestIter>();
        if iter.fail_at == Some(iter.index) {
            return -1;
        }
        let Some((key, value)) = iter.entries.get(iter.index) else {
            return 1;
        };
        if !write_buffer(key, out_key, out_key_len) || !write_buffer(value, out_val, out_val_len) {
            return -1;
        }
        iter.index += 1;
        0
    }

    unsafe extern "C" fn test_iter_free(iter: *mut c_void) {
        drop(Box::from_raw(iter.cast::<TestIter>()));
    }

    unsafe extern "C" fn test_free_buffer(buffer: *mut u8, _len: usize) {
        if !buffer.is_null() {
            libc::free(buffer.cast());
        }
    }

    fn callbacks(store: &mut TestKv) -> KvCallbacks {
        KvCallbacks {
            ctx: (store as *mut TestKv).cast(),
            get: test_get,
            set: test_set,
            delete: test_delete,
            iter_create: test_iter_create,
            iter_next: test_iter_next,
            iter_free: test_iter_free,
            free_buf: test_free_buffer,
        }
    }

    fn insert_checkpoint(store: &mut TestKv, id: u32) {
        store.entries.insert(
            checkpoint_key(id).to_vec(),
            write_checkpoint(&Checkpoint::tree_empty()),
        );
    }

    #[test]
    fn iterator_creation_failure_is_not_an_empty_checkpoint_set() {
        let mut state = TestKv {
            fail_iter_create: true,
            ..TestKv::default()
        };
        let store = KvShardStore::new(callbacks(&mut state));

        assert_eq!(store.max_checkpoint_id(), Err(KvError::IoError));
        assert!(matches!(
            TreeServer::new(callbacks(&mut state), 0),
            Err(KvError::IoError)
        ));
    }

    #[test]
    fn iterator_advance_failure_aborts_checkpoint_scans() {
        let mut state = TestKv {
            fail_iter_at: Some(1),
            ..TestKv::default()
        };
        insert_checkpoint(&mut state, 4);
        insert_checkpoint(&mut state, 9);
        let store = KvShardStore::new(callbacks(&mut state));

        assert_eq!(store.checkpoint_count(), Err(KvError::IoError));
        assert_eq!(store.max_checkpoint_id(), Err(KvError::IoError));
    }

    #[test]
    fn iterator_failure_does_not_partially_truncate_checkpoints() {
        let mut state = TestKv {
            fail_iter_at: Some(1),
            ..TestKv::default()
        };
        insert_checkpoint(&mut state, 1);
        insert_checkpoint(&mut state, 2);
        insert_checkpoint(&mut state, 3);
        let mut store = KvShardStore::new(callbacks(&mut state));

        assert_eq!(
            store.truncate_checkpoints_retaining(&0),
            Err(KvError::IoError)
        );
        assert!(state.entries.contains_key(checkpoint_key(1).as_slice()));
        assert!(state.entries.contains_key(checkpoint_key(2).as_slice()));
        assert!(state.entries.contains_key(checkpoint_key(3).as_slice()));
    }

    #[test]
    fn checkpoint_truncation_discards_only_newer_checkpoints() {
        let mut state = TestKv::default();
        insert_checkpoint(&mut state, 1);
        insert_checkpoint(&mut state, 2);
        insert_checkpoint(&mut state, 3);
        let mut store = KvShardStore::new(callbacks(&mut state));

        store.truncate_checkpoints_retaining(&2).unwrap();

        assert!(state.entries.contains_key(checkpoint_key(1).as_slice()));
        assert!(state.entries.contains_key(checkpoint_key(2).as_slice()));
        assert!(!state.entries.contains_key(checkpoint_key(3).as_slice()));
    }

    #[test]
    fn checkpoint_discovery_skips_noncanonical_keys_and_finds_the_true_maximum() {
        let mut state = TestKv::default();
        state
            .entries
            .insert(vec![CHECKPOINT_PREFIX, 0xff], vec![0xff]);
        state
            .entries
            .insert(vec![CHECKPOINT_PREFIX, 0, 0, 0, 99, 0], vec![0xff]);
        insert_checkpoint(&mut state, 3);
        insert_checkpoint(&mut state, 11);
        let store = KvShardStore::new(callbacks(&mut state));

        assert_eq!(store.min_checkpoint_id().unwrap(), Some(3));
        assert_eq!(store.max_checkpoint_id().unwrap(), Some(11));
        assert_eq!(store.checkpoint_count().unwrap(), 2);
    }

    #[test]
    fn corrupt_checkpoint_is_not_reported_as_missing() {
        let mut state = TestKv::default();
        state.entries.insert(checkpoint_key(5).to_vec(), vec![0xff]);
        let store = KvShardStore::new(callbacks(&mut state));

        assert!(matches!(
            store.get_checkpoint(&5),
            Err(KvError::Deserialization)
        ));
        assert_eq!(store.max_checkpoint_id(), Err(KvError::Deserialization));
    }

    #[test]
    fn invalid_shard_location_is_not_reported_as_missing() {
        let mut state = TestKv::default();
        let mut tree: PrunableTree<MerkleHashVote> = Tree::empty();
        for _ in 0..=SHARD_HEIGHT {
            tree = Tree::parent(None, tree, Tree::empty());
        }
        state
            .entries
            .insert(shard_key(0).to_vec(), write_shard_vote(&tree).unwrap());
        let store = KvShardStore::new(callbacks(&mut state));
        let address = Address::from_parts(Level::from(SHARD_HEIGHT), 0);

        assert_eq!(store.get_shard(address), Err(KvError::Deserialization));
    }

    #[test]
    fn tree_queries_propagate_checkpoint_storage_failure() {
        let mut state = TestKv::default();
        insert_checkpoint(&mut state, 7);
        let tree = TreeServer::new(callbacks(&mut state), 0).unwrap();
        state.fail_get = true;

        assert!(matches!(
            tree.root(),
            Err(ShardTreeError::Storage(KvError::IoError))
        ));
        assert!(matches!(
            tree.root_at_height(7),
            Err(ShardTreeError::Storage(KvError::IoError))
        ));
        assert!(matches!(
            tree.path(0, 7),
            Err(ShardTreeError::Storage(KvError::IoError))
        ));

        let syncable = SyncableServer::new(tree);
        assert!(matches!(
            syncable.get_tree_state(),
            Err(ShardTreeError::Storage(KvError::IoError))
        ));
    }
}
