// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Single account in the system.

use std::collections::hash_map::Entry;
use util::*;
use pod_account::*;
use rlp::*;
use lru_cache::LruCache;

use std::cell::{RefCell, Cell};

const STORAGE_CACHE_ITEMS: usize = 4096;

/// Single account in the system.
pub struct Account {
	// Balance of the account.
	balance: U256,
	// Nonce of the account.
	nonce: U256,
	// Trie-backed storage.
	storage_root: H256,
	// LRU Cache of the trie-backed storage.
	// This is limited to `STORAGE_CACHE_ITEMS` recent queries
	storage_cache: RefCell<LruCache<H256, H256>>,
	// Modified storage. Accumulates changes to storage made in `set_storage`
	// Takes precedence over `storage_cache`.
	storage_changes: HashMap<H256, H256>,
	// Code hash of the account. If None, means that it's a contract whose code has not yet been set.
	code_hash: Option<H256>,
	// Size of the accoun code.
	code_size: Option<usize>,
	// Code cache of the account.
	code_cache: Bytes,
	// Account is new or has been modified
	filth: Filth,
	// Cached address hash.
	address_hash: Cell<Option<H256>>,
}

impl Account {
	#[cfg(test)]
	/// General constructor.
	pub fn new(balance: U256, nonce: U256, storage: HashMap<H256, H256>, code: Bytes) -> Account {
		Account {
			balance: balance,
			nonce: nonce,
			storage_root: SHA3_NULL_RLP,
			storage_cache: Self::empty_storage_cache(),
			storage_changes: storage,
			code_hash: Some(code.sha3()),
			code_size: Some(code.len()),
			code_cache: code,
			filth: Filth::Dirty,
			address_hash: Cell::new(None),
		}
	}

	fn empty_storage_cache() -> RefCell<LruCache<H256, H256>> {
		RefCell::new(LruCache::new(STORAGE_CACHE_ITEMS))
	}

	/// General constructor.
	pub fn from_pod(pod: PodAccount) -> Account {
		Account {
			balance: pod.balance,
			nonce: pod.nonce,
			storage_root: SHA3_NULL_RLP,
			storage_cache: Self::empty_storage_cache(),
			storage_changes: pod.storage.into_iter().collect(),
			code_hash: pod.code.as_ref().map(|c| c.sha3()),
			code_size: Some(pod.code.as_ref().map_or(0, |c| c.len())),
			code_cache: pod.code.map_or_else(|| { warn!("POD account with unknown code is being created! Assuming no code."); vec![] }, |c| c),
			filth: Filth::Dirty,
			address_hash: Cell::new(None),
		}
	}

	/// Create a new account with the given balance.
	pub fn new_basic(balance: U256, nonce: U256) -> Account {
		Account {
			balance: balance,
			nonce: nonce,
			storage_root: SHA3_NULL_RLP,
			storage_cache: Self::empty_storage_cache(),
			storage_changes: HashMap::new(),
			code_hash: Some(SHA3_EMPTY),
			code_cache: vec![],
			code_size: Some(0),
			filth: Filth::Dirty,
			address_hash: Cell::new(None),
		}
	}

	/// Create a new account from RLP.
	pub fn from_rlp(rlp: &[u8]) -> Account {
		let r: Rlp = Rlp::new(rlp);
		Account {
			nonce: r.val_at(0),
			balance: r.val_at(1),
			storage_root: r.val_at(2),
			storage_cache: Self::empty_storage_cache(),
			storage_changes: HashMap::new(),
			code_hash: Some(r.val_at(3)),
			code_cache: vec![],
			code_size: None,
			filth: Filth::Clean,
			address_hash: Cell::new(None),
		}
	}

	/// Create a new contract account.
	/// NOTE: make sure you use `init_code` on this before `commit`ing.
	pub fn new_contract(balance: U256, nonce: U256) -> Account {
		Account {
			balance: balance,
			nonce: nonce,
			storage_root: SHA3_NULL_RLP,
			storage_cache: Self::empty_storage_cache(),
			storage_changes: HashMap::new(),
			code_hash: None,
			code_cache: vec![],
			code_size: None,
			filth: Filth::Dirty,
			address_hash: Cell::new(None),
		}
	}

	/// Set this account's code to the given code.
	/// NOTE: Account should have been created with `new_contract()`
	pub fn init_code(&mut self, code: Bytes) {
		assert!(self.code_hash.is_none());
		self.code_cache = code;
		self.code_size = Some(self.code_cache.len());
		self.filth = Filth::Dirty;
	}

	/// Reset this account's code to the given code.
	pub fn reset_code(&mut self, code: Bytes) {
		self.code_hash = None;
		self.code_size = Some(0);
		self.init_code(code);
	}

	/// Set (and cache) the contents of the trie's storage at `key` to `value`.
	pub fn set_storage(&mut self, key: H256, value: H256) {
		match self.storage_changes.entry(key) {
			Entry::Occupied(ref mut entry) if entry.get() != &value => {
				entry.insert(value);
				self.filth = Filth::Dirty;
			},
			Entry::Vacant(entry) => {
				entry.insert(value);
				self.filth = Filth::Dirty;
			},
			_ => {},
		}
	}

	/// Get (and cache) the contents of the trie's storage at `key`.
	/// Takes modifed storage into account.
	pub fn storage_at(&self, db: &HashDB, key: &H256) -> H256 {
		if let Some(value) = self.cached_storage_at(key) {
			return value;
		}
		let db = SecTrieDB::new(db, &self.storage_root)
			.expect("Account storage_root initially set to zero (valid) and only altered by SecTrieDBMut. \
			SecTrieDBMut would not set it to an invalid state root. Therefore the root is valid and DB creation \
			using it will not fail.");

		let item: U256 = match db.get(key){
			Ok(x) => x.map_or_else(U256::zero, decode),
			Err(e) => panic!("Encountered potential DB corruption: {}", e),
		};
		let value: H256 = item.into();
		self.storage_cache.borrow_mut().insert(key.clone(), value.clone());
		value
	}

	/// Get cached storage value if any. Returns `None` if the
	/// key is not in the cache.
	pub fn cached_storage_at(&self, key: &H256) -> Option<H256> {
		if let Some(value) = self.storage_changes.get(key) {
			return Some(value.clone())
		}
		if let Some(value) = self.storage_cache.borrow_mut().get_mut(key) {
			return Some(value.clone())
		}
		None
	}

	/// return the balance associated with this account.
	pub fn balance(&self) -> &U256 { &self.balance }

	/// return the nonce associated with this account.
	pub fn nonce(&self) -> &U256 { &self.nonce }

	#[cfg(test)]
	/// return the code hash associated with this account.
	pub fn code_hash(&self) -> H256 {
		self.code_hash.clone().unwrap_or(SHA3_EMPTY)
	}

	/// return the code hash associated with this account.
	pub fn address_hash(&self, address: &Address) -> H256 {
		let hash = self.address_hash.get();
		hash.unwrap_or_else(|| {
			let hash = address.sha3();
			self.address_hash.set(Some(hash.clone()));
			hash
		})
	}

	/// returns the account's code. If `None` then the code cache isn't available -
	/// get someone who knows to call `note_code`.
	pub fn code(&self) -> Option<&[u8]> {
		match self.code_hash {
			Some(c) if c == SHA3_EMPTY && self.code_cache.is_empty() => Some(&self.code_cache),
			Some(_) if !self.code_cache.is_empty() => Some(&self.code_cache),
			None => Some(&self.code_cache),
			_ => None,
		}
	}

	/// returns the account's code size. If `None` then the code cache or code size cache isn't available -
	/// get someone who knows to call `note_code`.
	pub fn code_size(&self) -> Option<usize> {
		self.code_size.clone()
	}

	#[cfg(test)]
	/// Provide a byte array which hashes to the `code_hash`. returns the hash as a result.
	pub fn note_code(&mut self, code: Bytes) -> Result<(), H256> {
		let h = code.sha3();
		match self.code_hash {
			Some(ref i) if h == *i => {
				self.code_cache = code;
				self.code_size = Some(self.code_cache.len());
				Ok(())
			},
			_ => Err(h)
		}
	}

	/// Is `code_cache` valid; such that code is going to return Some?
	pub fn is_cached(&self) -> bool {
		!self.code_cache.is_empty() || (self.code_cache.is_empty() && self.code_hash == Some(SHA3_EMPTY))
	}

	/// Is this a new or modified account?
	pub fn is_dirty(&self) -> bool {
		self.filth == Filth::Dirty || !self.storage_is_clean()
	}

	/// Mark account as clean.
	pub fn set_clean(&mut self) {
		assert!(self.storage_is_clean());
		self.filth = Filth::Clean
	}

	/// Provide a database to get `code_hash`. Should not be called if it is a contract without code.
	pub fn cache_code(&mut self, db: &HashDB) -> bool {
		// TODO: fill out self.code_cache;
		trace!("Account::cache_code: ic={}; self.code_hash={:?}, self.code_cache={}", self.is_cached(), self.code_hash, self.code_cache.pretty());
		self.is_cached() ||
			match self.code_hash {
				Some(ref h) => match db.get(h) {
					Some(x) => {
						self.code_cache = x.to_vec();
						self.code_size = Some(x.len());
						true
					},
					_ => {
						warn!("Failed reverse get of {}", h);
						false
					},
				},
				_ => false,
			}
	}

	/// Provide a database to get `code_size`. Should not be called if it is a contract without code.
	pub fn cache_code_size(&mut self, db: &HashDB) -> bool {
		// TODO: fill out self.code_cache;
		trace!("Account::cache_code_size: ic={}; self.code_hash={:?}, self.code_cache={}", self.is_cached(), self.code_hash, self.code_cache.pretty());
		self.code_size.is_some() ||
			match self.code_hash {
				Some(ref h) if h != &SHA3_EMPTY => match db.get(h) {
					Some(x) => {
						self.code_size = Some(x.len());
						true
					},
					_ => {
						warn!("Failed reverse get of {}", h);
						false
					},
				},
				_ => false,
			}
	}

	/// Determine whether there are any un-`commit()`-ed storage-setting operations.
	pub fn storage_is_clean(&self) -> bool { self.storage_changes.is_empty() }

	#[cfg(test)]
	/// return the storage root associated with this account or None if it has been altered via the overlay.
	pub fn storage_root(&self) -> Option<&H256> { if self.storage_is_clean() {Some(&self.storage_root)} else {None} }

	/// return the storage overlay.
	pub fn storage_changes(&self) -> &HashMap<H256, H256> { &self.storage_changes }

	/// Increment the nonce of the account by one.
	pub fn inc_nonce(&mut self) {
		self.nonce = self.nonce + U256::from(1u8);
		self.filth = Filth::Dirty;
	}

	/// Increment the nonce of the account by one.
	pub fn add_balance(&mut self, x: &U256) {
		if !x.is_zero() {
			self.balance = self.balance + *x;
			self.filth = Filth::Dirty;
		}
	}

	/// Increment the nonce of the account by one.
	/// Panics if balance is less than `x`
	pub fn sub_balance(&mut self, x: &U256) {
		if !x.is_zero() {
			assert!(self.balance >= *x);
			self.balance = self.balance - *x;
			self.filth = Filth::Dirty;
		}
	}

	/// Commit the `storage_changes` to the backing DB and update `storage_root`.
	pub fn commit_storage(&mut self, trie_factory: &TrieFactory, db: &mut HashDB) {
		let mut t = trie_factory.from_existing(db, &mut self.storage_root)
			.expect("Account storage_root initially set to zero (valid) and only altered by SecTrieDBMut. \
				SecTrieDBMut would not set it to an invalid state root. Therefore the root is valid and DB creation \
				using it will not fail.");
		for (k, v) in self.storage_changes.drain() {
			// cast key and value to trait type,
			// so we can call overloaded `to_bytes` method
			let res = match v.is_zero() {
				true => t.remove(&k),
				false => t.insert(&k, &encode(&U256::from(&*v))),
			};

			if let Err(e) = res {
				warn!("Encountered potential DB corruption: {}", e);
			}
			self.storage_cache.borrow_mut().insert(k, v);
		}
	}

	/// Commit any unsaved code. `code_hash` will always return the hash of the `code_cache` after this.
	pub fn commit_code(&mut self, db: &mut HashDB) {
		trace!("Commiting code of {:?} - {:?}, {:?}", self, self.code_hash.is_none(), self.code_cache.is_empty());
		match (self.code_hash.is_none(), self.code_cache.is_empty()) {
			(true, true) => {
				self.code_hash = Some(SHA3_EMPTY);
				self.code_size = Some(0);
			},
			(true, false) => {
				self.code_hash = Some(db.insert(&self.code_cache));
				self.code_size = Some(self.code_cache.len());
			},
			(false, _) => {},
		}
	}

	/// Export to RLP.
	pub fn rlp(&self) -> Bytes {
		let mut stream = RlpStream::new_list(4);
		stream.append(&self.nonce);
		stream.append(&self.balance);
		stream.append(&self.storage_root);
		stream.append(self.code_hash.as_ref().unwrap_or(&SHA3_EMPTY));
		stream.out()
	}

	/// Clone basic account data
	pub fn clone_basic(&self) -> Account {
		Account {
			balance: self.balance.clone(),
			nonce: self.nonce.clone(),
			storage_root: self.storage_root.clone(),
			storage_cache: Self::empty_storage_cache(),
			storage_changes: HashMap::new(),
			code_hash: self.code_hash.clone(),
			code_size: self.code_size.clone(),
			code_cache: Bytes::new(),
			filth: self.filth,
			address_hash: self.address_hash.clone(),
		}
	}

	/// Clone account data and dirty storage keys
	pub fn clone_dirty(&self) -> Account {
		let mut account = self.clone_basic();
		account.storage_changes = self.storage_changes.clone();
		account.code_cache = self.code_cache.clone();
		account
	}

	/// Clone account data, dirty storage keys and cached storage keys.
	pub fn clone_all(&self) -> Account {
		let mut account = self.clone_dirty();
		account.storage_cache = self.storage_cache.clone();
		account
	}

	/// Replace self with the data from other account merging storage cache
	pub fn merge_with(&mut self, other: Account) {
		assert!(self.storage_is_clean());
		assert!(other.storage_is_clean());
		self.balance = other.balance;
		self.nonce = other.nonce;
		self.storage_root = other.storage_root;
		self.code_hash = other.code_hash;
		self.code_cache = other.code_cache;
		self.code_size = other.code_size;
		self.address_hash = other.address_hash;
		let mut cache = self.storage_cache.borrow_mut();
		for (k, v) in other.storage_cache.into_inner().into_iter() {
			cache.insert(k.clone() , v.clone()); //TODO: cloning should not be required here
		}
	}
}

impl fmt::Debug for Account {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{:?}", PodAccount::from_account(self))
	}
}

#[cfg(test)]
mod tests {

	use util::*;
	use super::*;
	use account_db::*;
	use rlp::*;

	#[test]
	fn account_compress() {
		let raw = Account::new_basic(2.into(), 4.into()).rlp();
		let rlp = UntrustedRlp::new(&raw);
		let compact_vec = rlp.compress(RlpType::Snapshot).to_vec();
		assert!(raw.len() > compact_vec.len());
		let again_raw = UntrustedRlp::new(&compact_vec).decompress(RlpType::Snapshot);
		assert_eq!(raw, again_raw.to_vec());
    }

	#[test]
	fn storage_at() {
		let mut db = MemoryDB::new();
		let mut db = AccountDBMut::new(&mut db, &Address::new());
		let rlp = {
			let mut a = Account::new_contract(69.into(), 0.into());
			a.set_storage(H256::from(&U256::from(0x00u64)), H256::from(&U256::from(0x1234u64)));
			a.commit_storage(&Default::default(), &mut db);
			a.init_code(vec![]);
			a.commit_code(&mut db);
			a.rlp()
		};

		let a = Account::from_rlp(&rlp);
		assert_eq!(a.storage_root().unwrap().hex(), "c57e1afb758b07f8d2c8f13a3b6e44fa5ff94ab266facc5a4fd3f062426e50b2");
		assert_eq!(a.storage_at(&db.immutable(), &H256::from(&U256::from(0x00u64))), H256::from(&U256::from(0x1234u64)));
		assert_eq!(a.storage_at(&db.immutable(), &H256::from(&U256::from(0x01u64))), H256::new());
	}

	#[test]
	fn note_code() {
		let mut db = MemoryDB::new();
		let mut db = AccountDBMut::new(&mut db, &Address::new());

		let rlp = {
			let mut a = Account::new_contract(69.into(), 0.into());
			a.init_code(vec![0x55, 0x44, 0xffu8]);
			a.commit_code(&mut db);
			a.rlp()
		};

		let mut a = Account::from_rlp(&rlp);
		assert!(a.cache_code(&db.immutable()));

		let mut a = Account::from_rlp(&rlp);
		assert_eq!(a.note_code(vec![0x55, 0x44, 0xffu8]), Ok(()));
	}

	#[test]
	fn commit_storage() {
		let mut a = Account::new_contract(69.into(), 0.into());
		let mut db = MemoryDB::new();
		let mut db = AccountDBMut::new(&mut db, &Address::new());
		a.set_storage(0.into(), 0x1234.into());
		assert_eq!(a.storage_root(), None);
		a.commit_storage(&Default::default(), &mut db);
		assert_eq!(a.storage_root().unwrap().hex(), "c57e1afb758b07f8d2c8f13a3b6e44fa5ff94ab266facc5a4fd3f062426e50b2");
	}

	#[test]
	fn commit_remove_commit_storage() {
		let mut a = Account::new_contract(69.into(), 0.into());
		let mut db = MemoryDB::new();
		let mut db = AccountDBMut::new(&mut db, &Address::new());
		a.set_storage(0.into(), 0x1234.into());
		a.commit_storage(&Default::default(), &mut db);
		a.set_storage(1.into(), 0x1234.into());
		a.commit_storage(&Default::default(), &mut db);
		a.set_storage(1.into(), 0.into());
		a.commit_storage(&Default::default(), &mut db);
		assert_eq!(a.storage_root().unwrap().hex(), "c57e1afb758b07f8d2c8f13a3b6e44fa5ff94ab266facc5a4fd3f062426e50b2");
	}

	#[test]
	fn commit_code() {
		let mut a = Account::new_contract(69.into(), 0.into());
		let mut db = MemoryDB::new();
		let mut db = AccountDBMut::new(&mut db, &Address::new());
		a.init_code(vec![0x55, 0x44, 0xffu8]);
		assert_eq!(a.code_hash(), SHA3_EMPTY);
		assert_eq!(a.code_size(), Some(3));
		a.commit_code(&mut db);
		assert_eq!(a.code_hash().hex(), "af231e631776a517ca23125370d542873eca1fb4d613ed9b5d5335a46ae5b7eb");
	}

	#[test]
	fn reset_code() {
		let mut a = Account::new_contract(69.into(), 0.into());
		let mut db = MemoryDB::new();
		let mut db = AccountDBMut::new(&mut db, &Address::new());
		a.init_code(vec![0x55, 0x44, 0xffu8]);
		assert_eq!(a.code_hash(), SHA3_EMPTY);
		a.commit_code(&mut db);
		assert_eq!(a.code_hash().hex(), "af231e631776a517ca23125370d542873eca1fb4d613ed9b5d5335a46ae5b7eb");
		a.reset_code(vec![0x55]);
		assert_eq!(a.code_hash(), SHA3_EMPTY);
		a.commit_code(&mut db);
		assert_eq!(a.code_hash().hex(), "37bf2238b11b68cdc8382cece82651b59d3c3988873b6e0f33d79694aa45f1be");
	}

	#[test]
	fn rlpio() {
		let a = Account::new(U256::from(69u8), U256::from(0u8), HashMap::new(), Bytes::new());
		let b = Account::from_rlp(&a.rlp());
		assert_eq!(a.balance(), b.balance());
		assert_eq!(a.nonce(), b.nonce());
		assert_eq!(a.code_hash(), b.code_hash());
		assert_eq!(a.storage_root(), b.storage_root());
	}

	#[test]
	fn new_account() {
		let a = Account::new(U256::from(69u8), U256::from(0u8), HashMap::new(), Bytes::new());
		assert_eq!(a.rlp().to_hex(), "f8448045a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");
		assert_eq!(a.balance(), &U256::from(69u8));
		assert_eq!(a.nonce(), &U256::from(0u8));
		assert_eq!(a.code_hash(), SHA3_EMPTY);
		assert_eq!(a.storage_root().unwrap(), &SHA3_NULL_RLP);
	}

	#[test]
	fn create_account() {
		let a = Account::new(U256::from(69u8), U256::from(0u8), HashMap::new(), Bytes::new());
		assert_eq!(a.rlp().to_hex(), "f8448045a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");
	}

}
