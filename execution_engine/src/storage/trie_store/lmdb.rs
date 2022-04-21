//! An LMDB-backed trie store.
//!
//! # Usage
//!
//! ```
//! use casper_execution_engine::storage::store::Store;
//! use casper_execution_engine::storage::transaction_source::{Transaction, TransactionSource};
//! use casper_execution_engine::storage::transaction_source::lmdb::LmdbEnvironment;
//! use casper_execution_engine::storage::trie::{Pointer, PointerBlock, Trie};
//! use casper_execution_engine::storage::trie_store::lmdb::LmdbTrieStore;
//! use casper_hashing::Digest;
//! use casper_types::bytesrepr::{ToBytes, Bytes};
//! use lmdb::DatabaseFlags;
//! use tempfile::tempdir;
//!
//! // Create some leaves
//! let leaf_1 = Trie::Leaf { key: Bytes::from(vec![0u8, 0, 0]), value: Bytes::from(b"val_1".to_vec()) };
//! let leaf_2 = Trie::Leaf { key: Bytes::from(vec![1u8, 0, 0]), value: Bytes::from(b"val_2".to_vec()) };
//!
//! // Get their hashes
//! let leaf_1_hash = Digest::hash(&leaf_1.to_bytes().unwrap());
//! let leaf_2_hash = Digest::hash(&leaf_2.to_bytes().unwrap());
//!
//! // Create a node
//! let node: Trie<Bytes, Bytes> = {
//!     let mut pointer_block = PointerBlock::new();
//!     pointer_block[0] = Some(Pointer::LeafPointer(leaf_1_hash));
//!     pointer_block[1] = Some(Pointer::LeafPointer(leaf_2_hash));
//!     let pointer_block = Box::new(pointer_block);
//!     Trie::Node { pointer_block }
//! };
//!
//! // Get its hash
//! let node_hash = Digest::hash(&node.to_bytes().unwrap());
//!
//! // Create the environment and the store. For both the in-memory and
//! // LMDB-backed implementations, the environment is the source of
//! // transactions.
//! let tmp_dir = tempdir().unwrap();
//! let map_size = 4096 * 2560;  // map size should be a multiple of OS page size
//! let max_readers = 512;
//! let env = LmdbEnvironment::new(&tmp_dir.path().to_path_buf(), map_size, max_readers, true).unwrap();
//! let store = LmdbTrieStore::new(&env, None, DatabaseFlags::empty()).unwrap();
//!
//! // First let's create a read-write transaction, persist the values, but
//! // forget to commit the transaction.
//! {
//!     // Create a read-write transaction
//!     let mut txn = env.create_read_write_txn().unwrap();
//!
//!     // Put the values in the store
//!     store.put(&mut txn, &leaf_1_hash, &leaf_1).unwrap();
//!     store.put(&mut txn, &leaf_2_hash, &leaf_2).unwrap();
//!     store.put(&mut txn, &node_hash, &node).unwrap();
//!
//!     // Here we forget to commit the transaction before it goes out of scope
//! }
//!
//! // Now let's check to see if the values were stored
//! {
//!     // Create a read transaction
//!     let txn = env.create_read_txn().unwrap();
//!
//!     // Observe that nothing has been persisted to the store
//!     for hash in vec![&leaf_1_hash, &leaf_2_hash, &node_hash].iter() {
//!         // We need to use a type annotation here to help the compiler choose
//!         // a suitable FromBytes instance
//!         let maybe_trie: Option<Trie<Bytes, Bytes>> = store.get(&txn, hash).unwrap();
//!         assert!(maybe_trie.is_none());
//!     }
//!
//!     // Commit the read transaction.  Not strictly necessary, but better to be hygienic.
//!     txn.commit().unwrap();
//! }
//!
//! // Now let's try that again, remembering to commit the transaction this time
//! {
//!     // Create a read-write transaction
//!     let mut txn = env.create_read_write_txn().unwrap();
//!
//!     // Put the values in the store
//!     store.put(&mut txn, &leaf_1_hash, &leaf_1).unwrap();
//!     store.put(&mut txn, &leaf_2_hash, &leaf_2).unwrap();
//!     store.put(&mut txn, &node_hash, &node).unwrap();
//!
//!     // Commit the transaction.
//!     txn.commit().unwrap();
//! }
//!
//! // Now let's check to see if the values were stored again
//! {
//!     // Create a read transaction
//!     let txn = env.create_read_txn().unwrap();
//!
//!     // Get the values in the store
//!     assert_eq!(Some(leaf_1), store.get(&txn, &leaf_1_hash).unwrap());
//!     assert_eq!(Some(leaf_2), store.get(&txn, &leaf_2_hash).unwrap());
//!     assert_eq!(Some(node), store.get(&txn, &node_hash).unwrap());
//!
//!     // Commit the read transaction.
//!     txn.commit().unwrap();
//! }
//!
//! tmp_dir.close().unwrap();
//! ```
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use casper_types::{bytesrepr, Key, StoredValue};
use lmdb::{Database, DatabaseFlags, Transaction};

use casper_hashing::Digest;

use crate::storage::{
    error,
    global_state::CommitError,
    store::Store,
    transaction_source::{lmdb::LmdbEnvironment, Readable, TransactionSource, Writable},
    trie::Trie,
    trie_store::{self, TrieStore},
};

/// An LMDB-backed trie store.
///
/// Wraps [`lmdb::Database`].
#[derive(Debug, Clone)]
pub struct LmdbTrieStore {
    db: Database,
}

impl LmdbTrieStore {
    /// Constructor for new `LmdbTrieStore`.
    pub fn new(
        env: &LmdbEnvironment,
        maybe_name: Option<&str>,
        flags: DatabaseFlags,
    ) -> Result<Self, error::Error> {
        let name = Self::name(maybe_name);
        let db = env.env().create_db(Some(&name), flags)?;
        Ok(LmdbTrieStore { db })
    }

    /// Constructor for `LmdbTrieStore` which opens an existing lmdb store file.
    pub fn open(env: &LmdbEnvironment, maybe_name: Option<&str>) -> Result<Self, error::Error> {
        let name = Self::name(maybe_name);
        let db = env.env().open_db(Some(&name))?;
        Ok(LmdbTrieStore { db })
    }

    fn name(maybe_name: Option<&str>) -> String {
        maybe_name
            .map(|name| format!("{}-{}", trie_store::NAME, name))
            .unwrap_or_else(|| String::from(trie_store::NAME))
    }

    /// Get a handle to the underlying database.
    pub fn get_db(&self) -> Database {
        self.db
    }
}

impl<K, V> Store<Digest, Trie<K, V>> for LmdbTrieStore {
    type Error = error::Error;

    type Handle = Database;

    fn handle(&self) -> Self::Handle {
        self.db
    }
}

impl<K, V> TrieStore<K, V> for LmdbTrieStore {}

pub(crate) type Cache = Arc<Mutex<HashMap<Digest, (bool, Trie<Key, StoredValue>)>>>;
/// In-memory cached trie store, backed by rocksdb.
#[derive(Clone)]
pub struct ScratchCache {
    pub(crate) cache: Cache,
    pub(crate) store: Arc<LmdbTrieStore>,
    pub(crate) env: Arc<LmdbEnvironment>,
}

/// Cached version of the trie store.
#[derive(Clone)]
pub struct ScratchTrieStore {
    pub(crate) inner: ScratchCache,
}

impl ScratchTrieStore {
    /// Creates a new ScratchTrieStore.
    pub fn new(store: Arc<LmdbTrieStore>, env: Arc<LmdbEnvironment>) -> Self {
        Self {
            inner: ScratchCache {
                store,
                env,
                cache: Default::default(),
            },
        }
    }

    /// Writes only (dirty) tries under the given `state_root` to the underlying db.
    pub fn write_root_to_db(self, state_root: Digest) -> Result<(), error::Error> {
        let env = self.inner.env;
        let store = self.inner.store;
        let cache = &mut *self.inner.cache.lock().map_err(|_| error::Error::Poison)?;

        let mut missing_trie_keys = vec![state_root];
        let mut validated_tries = HashMap::new();

        let mut txn = env.create_read_write_txn()?;

        while let Some(next_trie_key) = missing_trie_keys.pop() {
            if cache.is_empty() {
                return Err(error::Error::CommitError(
                    CommitError::TrieNotFoundDuringCacheValidate(next_trie_key),
                ));
            }
            match cache.remove(&next_trie_key) {
                Some((false, _)) => continue,
                None => {
                    txn.read(store.get_db(), next_trie_key.as_ref())?.ok_or(
                        error::Error::CommitError(CommitError::TrieNotFoundDuringCacheValidate(
                            next_trie_key,
                        )),
                    )?;
                }
                Some((true, trie)) => {
                    if let Some(children) = trie.children() {
                        missing_trie_keys.extend(children);
                    }
                    validated_tries.insert(next_trie_key, trie);
                }
            }
        }

        // after validating that all the needed tries are present, write everything
        for (digest, trie) in validated_tries.iter() {
            store.put(&mut txn, digest, trie)?;
        }

        // required for lmdb
        txn.commit()?;

        Ok(())
    }
}

impl Store<Digest, Trie<Key, StoredValue>> for ScratchTrieStore {
    type Error = error::Error;

    type Handle = ScratchCache;

    fn handle(&self) -> Self::Handle {
        self.inner.clone()
    }

    /// Puts a `value` into the store at `key` within a transaction, potentially returning an
    /// error of type `Self::Error` if that fails.
    fn put<T>(
        &self,
        _txn: &mut T,
        digest: &Digest,
        trie: &Trie<Key, StoredValue>,
    ) -> Result<(), Self::Error>
    where
        T: Writable<Handle = Self::Handle>,
        Self::Error: From<T::Error>,
    {
        self.inner
            .cache
            .lock()
            .map_err(|_| error::Error::Poison)?
            .insert(*digest, (true, trie.clone()));
        Ok(())
    }

    /// Returns an optional value (may exist or not) as read through a transaction, or an error
    /// of the associated `Self::Error` variety.
    fn get<T>(
        &self,
        txn: &T,
        digest: &Digest,
    ) -> Result<Option<Trie<Key, StoredValue>>, Self::Error>
    where
        T: Readable<Handle = Self::Handle>,
        Self::Error: From<T::Error>,
    {
        let maybe_trie = {
            self.inner
                .cache
                .lock()
                .map_err(|_| error::Error::Poison)?
                .get(digest)
                .cloned()
        };
        match maybe_trie {
            Some((_, cached)) => Ok(Some(cached)),
            None => {
                let raw = self.get_raw(txn, digest)?;
                match raw {
                    Some(bytes) => {
                        let value: Trie<Key, StoredValue> = bytesrepr::deserialize(bytes.into())?;
                        {
                            let store =
                                &mut *self.inner.cache.lock().map_err(|_| error::Error::Poison)?;
                            if !store.contains_key(digest) {
                                store.insert(*digest, (false, value.clone()));
                            }
                        }
                        Ok(Some(value))
                    }
                    None => Ok(None),
                }
            }
        }
    }
}

impl TrieStore<Key, StoredValue> for ScratchTrieStore {}
