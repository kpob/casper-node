use std::{
    cmp::Ordering,
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
};

use rand::Rng;

use casper_engine_test_support::LmdbWasmTestBuilder;
use casper_hashing::Digest;
use casper_types::{
    account::{Account, AccountHash},
    system::{
        auction::{Bid, Bids, SeigniorageRecipientsSnapshot, SEIGNIORAGE_RECIPIENTS_SNAPSHOT_KEY},
        mint::TOTAL_SUPPLY_KEY,
    },
    AccessRights, CLValue, Key, PublicKey, StoredValue, URef, U512,
};

use crate::utils::print_entry;

use super::config::Transfer;

/// A struct tracking changes to be made to the global state.
pub struct StateTracker {
    builder: LmdbWasmTestBuilder,
    entries_to_write: BTreeMap<Key, StoredValue>,
    total_supply: U512,
    total_supply_key: Key,
    accounts_cache: BTreeMap<AccountHash, Account>,
    purses_cache: BTreeMap<URef, U512>,
    bids_cache: Option<Bids>,
}

impl StateTracker {
    /// Creates a new `StateTracker`.
    pub fn new(data_dir: &str, state_hash: Digest) -> Self {
        // Open the global state that should be in the supplied directory.
        let builder = LmdbWasmTestBuilder::open_raw(data_dir, Default::default(), state_hash);

        // Find the hash of the mint contract.
        let mint_contract_hash = builder.get_system_mint_hash();
        // Read the URef under which total supply is stored.
        let total_supply_key = builder
            .get_contract(mint_contract_hash)
            .expect("mint should exist")
            .named_keys()[TOTAL_SUPPLY_KEY];
        // Read the total supply.
        let total_supply_sv = builder
            .query(None, total_supply_key, &[])
            .expect("should query");
        let total_supply = total_supply_sv
            .as_cl_value()
            .cloned()
            .expect("should be cl value");

        Self {
            builder,
            entries_to_write: Default::default(),
            total_supply_key,
            total_supply: total_supply.into_t().expect("should be U512"),
            accounts_cache: BTreeMap::new(),
            purses_cache: BTreeMap::new(),
            bids_cache: None,
        }
    }

    /// Prints all the writes to be made to the global state.
    pub fn print_all_entries(&self) {
        for (key, value) in &self.entries_to_write {
            print_entry(key, value);
        }
    }

    /// Stores a write of an entry in the global state.
    pub fn write_entry(&mut self, key: Key, value: StoredValue) {
        let _ = self.entries_to_write.insert(key, value);
    }

    /// Increases the total supply of the tokens in the network.
    pub fn increase_supply(&mut self, to_add: U512) {
        self.total_supply += to_add;
        self.write_entry(
            self.total_supply_key,
            StoredValue::CLValue(CLValue::from_t(self.total_supply).unwrap()),
        );
    }

    /// Decreases the total supply of the tokens in the network.
    pub fn decrease_supply(&mut self, to_sub: U512) {
        self.total_supply -= to_sub;
        self.write_entry(
            self.total_supply_key,
            StoredValue::CLValue(CLValue::from_t(self.total_supply).unwrap()),
        );
    }

    /// Creates a new purse containing the given amount of motes and returns its URef.
    pub fn create_purse(&mut self, amount: U512) -> URef {
        let mut rng = rand::thread_rng();
        let new_purse = URef::new(rng.gen(), AccessRights::READ_ADD_WRITE);

        // Purse URef pointing to `()` so that the owner cannot modify the purse directly.
        self.write_entry(Key::URef(new_purse), StoredValue::CLValue(CLValue::unit()));

        self.set_purse_balance(new_purse, amount);

        new_purse
    }

    /// Gets the balance of the purse, taking into account changes made during the update.
    pub fn get_purse_balance(&mut self, purse: URef) -> U512 {
        match self.purses_cache.get(&purse).cloned() {
            Some(amount) => amount,
            None => {
                let amount = self.builder.get_purse_balance(purse);
                self.purses_cache.insert(purse, amount);
                amount
            }
        }
    }

    /// Sets the balance of the purse.
    pub fn set_purse_balance(&mut self, purse: URef, balance: U512) {
        let current_balance = self.get_purse_balance(purse);

        match balance.cmp(&current_balance) {
            Ordering::Less => self.increase_supply(balance - current_balance),
            Ordering::Greater => self.decrease_supply(current_balance - balance),
            Ordering::Equal => return,
        }

        self.write_entry(
            Key::Balance(purse.addr()),
            StoredValue::CLValue(CLValue::from_t(balance).unwrap()),
        );
        self.purses_cache.insert(purse, balance);
    }

    /// Creates a new account for the given public key and seeds it with the given amount of
    /// tokens.
    pub fn create_account(&mut self, account_hash: AccountHash, amount: U512) -> Account {
        let main_purse = self.create_purse(amount);

        let account = Account::create(account_hash, Default::default(), main_purse);

        self.accounts_cache.insert(account_hash, account.clone());

        self.write_entry(
            Key::Account(account_hash),
            StoredValue::Account(account.clone()),
        );

        account
    }

    /// Gets the account for the given public key.
    pub fn get_account(&mut self, account_hash: &AccountHash) -> Option<Account> {
        match self.accounts_cache.entry(*account_hash) {
            Entry::Vacant(vac) => self
                .builder
                .get_account(*account_hash)
                .map(|account| vac.insert(account).clone()),
            Entry::Occupied(occupied) => Some(occupied.into_mut().clone()),
        }
    }

    pub fn execute_transfer(&mut self, transfer: &Transfer) {
        let from_account = if let Some(account) = self.get_account(&transfer.from) {
            account
        } else {
            eprintln!("\"from\" account doesn't exist; transfer: {:?}", transfer);
            return;
        };

        let to_account = if let Some(account) = self.get_account(&transfer.to) {
            account
        } else {
            self.create_account(transfer.to, U512::zero())
        };

        let from_balance = self.get_purse_balance(from_account.main_purse());

        if from_balance < transfer.amount {
            eprintln!(
                "\"from\" account balance insufficient; balance = {}, transfer = {:?}",
                from_balance, transfer
            );
            return;
        }

        let to_balance = self.get_purse_balance(to_account.main_purse());

        self.set_purse_balance(from_account.main_purse(), from_balance - transfer.amount);
        self.set_purse_balance(to_account.main_purse(), to_balance + transfer.amount);
    }

    /// Reads the `SeigniorageRecipientsSnapshot` stored in the global state.
    pub fn read_snapshot(&self) -> (Key, SeigniorageRecipientsSnapshot) {
        // Find the hash of the auction contract.
        let auction_contract_hash = self.builder.get_system_auction_hash();

        // Read the key under which the snapshot is stored.
        let validators_key = self
            .builder
            .get_contract(auction_contract_hash)
            .expect("auction should exist")
            .named_keys()[SEIGNIORAGE_RECIPIENTS_SNAPSHOT_KEY];

        // Decode the old snapshot.
        let stored_value = self
            .builder
            .query(None, validators_key, &[])
            .expect("should query");
        let cl_value = stored_value
            .as_cl_value()
            .cloned()
            .expect("should be cl value");
        (validators_key, cl_value.into_t().expect("should convert"))
    }

    /// Reads the bids from the global state.
    pub fn get_bids(&mut self) -> Bids {
        if let Some(ref bids) = self.bids_cache {
            bids.clone()
        } else {
            let bids = self.builder.get_bids();
            self.bids_cache = Some(bids.clone());
            bids
        }
    }

    /// Sets the bid for the given account.
    pub fn set_bid(&mut self, public_key: PublicKey, bid: Bid) {
        let maybe_current_bid = self.get_bids().get(&public_key).cloned();

        let new_amount = match bid.total_staked_amount() {
            Ok(amount) => amount,
            Err(e) => {
                eprintln!("invalid new bid for {:?}: {:?}", public_key, e);
                return;
            }
        };

        // we called `get_bids` above, so `bids_cache` will be `Some`
        self.bids_cache
            .as_mut()
            .unwrap()
            .insert(public_key.clone(), bid.clone());

        let account_hash = public_key.to_account_hash();

        // Replace the bid.
        self.write_entry(
            Key::Bid(account_hash),
            Bid::empty(public_key, *bid.bonding_purse()).into(),
        );

        // Update the bonding purses - this will also take care of the total supply changes.
        if let Some(old_bid) = maybe_current_bid {
            self.set_purse_balance(*old_bid.bonding_purse(), U512::zero());
        }
        self.set_purse_balance(*bid.bonding_purse(), new_amount);
    }

    /// Generates the writes to the global state that will remove the pending withdraws of all the
    /// old validators that will cease to be validators.
    pub fn remove_withdraws(&mut self, removed: &BTreeSet<PublicKey>) {
        let withdraws = self.builder.get_unbonds();
        let withdraw_keys: BTreeSet<_> = withdraws.keys().collect();
        for (key, value) in removed
            .iter()
            .map(PublicKey::to_account_hash)
            .filter(|acc| withdraw_keys.contains(&acc))
            .map(|acc| (Key::Withdraw(acc), StoredValue::Withdraw(vec![])))
        {
            self.write_entry(key, value);
        }
    }
}
