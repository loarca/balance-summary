use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::BufReader,
    ops::{Add, AddAssign, Sub, SubAssign},
    path::PathBuf,
};

use clap::Parser;
use csv::{ReaderBuilder, Trim};
use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    Csv(csv::Error),

    #[error("found unknown transaction type: {0}")]
    UnknownTransactionType(String),
    #[error("found transaction with missing amount")]
    MissingAmount,
}

#[derive(Debug, Clone, Copy)]
struct ClientId(u16);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TransactionId(u32);

/// Money amount with up to 4 decimal places
// NOTE: this could be swapped with the `bigdecimal` crate if we need to manage bigger numbers
// (very unlikely though) but this one from `rust_decimal` doesn't allocate on the heap
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Money(Decimal);
impl Money {
    fn new(decimal: Decimal) -> Self {
        // drop any decimal places after the 4th
        Self(decimal.trunc_with_scale(4))
    }
}
impl From<Decimal> for Money {
    fn from(value: Decimal) -> Self {
        Self::new(value)
    }
}
impl Add for Money {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}
impl AddAssign for Money {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0
    }
}
impl Sub for Money {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}
impl SubAssign for Money {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0
    }
}

#[derive(Deserialize)]
struct TransactionRecord {
    #[serde(rename = "type")]
    kind: String,
    client: u16,
    tx: u32,
    amount: Option<Decimal>,
}
impl TryFrom<TransactionRecord> for Transaction {
    type Error = Error;

    fn try_from(value: TransactionRecord) -> Result<Self, Self::Error> {
        let id = TransactionId(value.tx);
        let client_id = ClientId(value.client);
        match value.kind.as_str() {
            "deposit" => {
                let Some(amount) = value.amount else {
                    return Err(Error::MissingAmount);
                };
                Ok(Self {
                    id,
                    client_id,
                    kind: TransactionKind::Deposit {
                        amount: Money::new(amount),
                    },
                })
            }
            "withdrawal" => {
                let Some(amount) = value.amount else {
                    return Err(Error::MissingAmount);
                };
                Ok(Self {
                    id,
                    client_id,
                    kind: TransactionKind::Withdrawal {
                        amount: Money::new(amount),
                    },
                })
            }
            "dispute" => Ok(Self {
                id,
                client_id,
                kind: TransactionKind::Dispute,
            }),
            "resolve" => Ok(Self {
                id,
                client_id,
                kind: TransactionKind::Resolve,
            }),
            "chargeback" => Ok(Self {
                id,
                client_id,
                kind: TransactionKind::Chargeback,
            }),
            _ => Err(Error::UnknownTransactionType(value.kind)),
        }
    }
}

#[derive(Debug, Clone)]
struct Transaction {
    id: TransactionId,
    client_id: ClientId,
    kind: TransactionKind,
}
#[derive(Debug, Clone, Copy)]
enum TransactionKind {
    Deposit { amount: Money },
    Withdrawal { amount: Money },
    Dispute,
    Resolve,
    Chargeback,
}
impl TransactionKind {
    fn id_must_be_new(self) -> bool {
        match self {
            TransactionKind::Deposit { .. } | TransactionKind::Withdrawal { .. } => true,
            TransactionKind::Dispute | TransactionKind::Resolve | TransactionKind::Chargeback => {
                false
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Account {
    available: Money,
    held: Money,
    is_frozen: bool,
    deposit_info: HashMap<TransactionId, DepositInfo>,
}
#[derive(Debug, Clone)]
struct DepositInfo {
    amount: Money,
    status: DepositStatus,
}
#[derive(Debug, Clone, Copy)]
enum DepositStatus {
    Available,
    Held,
    Chargedback,
}

const MAX_CLIENTS: usize = u16::MAX as usize + 1;
/// Accounts indexed by their Client Id.
///
/// Since u16 is small enough we can look up by it as an index and avoid hashing.
/// Should be about 5.5MB in memory.
struct AccountsMap(Box<[Option<Account>; MAX_CLIENTS]>);
impl AccountsMap {
    fn new() -> Self {
        Self(
            vec![None; MAX_CLIENTS]
                .into_boxed_slice()
                .try_into()
                .expect("must be a valid boxed array"),
        )
    }
    fn get_or_create(&mut self, client_id: ClientId) -> &mut Account {
        self.0
            .get_mut(client_id.0 as usize)
            .expect("must exist because the array covers the whole id range")
            .get_or_insert(Account::default())
    }
}

/// Account Balance Engine: read transactions from CSV and calculate account balances
#[derive(clap::Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the transactions CSV file
    input: PathBuf,
}

fn main() -> Result<(), Error> {
    let cli = Cli::parse();

    // instead of loading the whole file in memory
    // we stream it and deserialize it in chunks
    //
    // this could be replaced with any Read type, like tcp streams or stdin
    let file = File::open(&cli.input).map_err(Error::Io)?;
    // 256KB buffer
    let buf_reader = BufReader::with_capacity(256 * 1024, file);
    let tx_records = ReaderBuilder::new()
        .has_headers(true)
        .delimiter(b',')
        .trim(Trim::All)
        .flexible(true)
        .from_reader(buf_reader)
        .into_deserialize::<TransactionRecord>();

    // used to enforce that transactions are globally unique
    // if we find duplicates those should be dropped
    let mut seen_tx: HashSet<TransactionId> = HashSet::new();

    let mut accounts = AccountsMap::new();

    for record in tx_records {
        let transaction: Transaction = record.map_err(Error::Csv)?.try_into()?;

        if transaction.kind.id_must_be_new() {
            // mark tx ids as seen only for non-dispute transactions
            let is_new_tx_id = seen_tx.insert(transaction.id);
            if !is_new_tx_id {
                // skip duplicate tx ids
                continue;
            }
        }

        let account = accounts.get_or_create(transaction.client_id);
        if account.is_frozen {
            // skip frozen accounts
            continue;
        }

        match transaction.kind {
            TransactionKind::Deposit { amount } => {
                account.available += amount;
                account.deposit_info.insert(
                    transaction.id,
                    DepositInfo {
                        amount,
                        status: DepositStatus::Available,
                    },
                );
            }
            TransactionKind::Withdrawal { amount } => {
                if account.available < amount {
                    // failed withdrawal
                    continue;
                }
                account.available -= amount;
            }
            TransactionKind::Dispute => {
                if let Some(deposit_info) = account.deposit_info.get_mut(&transaction.id) {
                    match deposit_info.status {
                        DepositStatus::Available => {
                            // while it wouldn't make much sense to allow negative available amounts
                            // users could still try to dispute after withdrawing funds
                            // in an attempt to commit fraud,
                            // those disputes must still be handled/investigated accordingly
                            //
                            // banks usually allow it but later demand to deposit new funds
                            // to make the balance positive again
                            account.available -= deposit_info.amount;
                            account.held += deposit_info.amount;
                            deposit_info.status = DepositStatus::Held;
                        }
                        DepositStatus::Held | DepositStatus::Chargedback => {
                            // should be unreachable, ignore
                        }
                    }
                } else {
                    // if that tx id doesn't exist then ignore
                }
            }
            TransactionKind::Resolve => {
                if let Some(deposit_info) = account.deposit_info.get_mut(&transaction.id) {
                    match deposit_info.status {
                        DepositStatus::Held => {
                            account.held -= deposit_info.amount;
                            account.available += deposit_info.amount;
                            deposit_info.status = DepositStatus::Available;
                        }
                        DepositStatus::Available | DepositStatus::Chargedback => {
                            // should be unreachable, ignore
                        }
                    }
                } else {
                    // if that tx id doesn't exist then ignore
                }
            }
            TransactionKind::Chargeback => {
                if let Some(deposit_info) = account.deposit_info.get_mut(&transaction.id) {
                    match deposit_info.status {
                        DepositStatus::Held => {
                            account.held -= deposit_info.amount;
                            deposit_info.status = DepositStatus::Chargedback;
                            account.is_frozen = true;
                        }
                        DepositStatus::Available | DepositStatus::Chargedback => {
                            // should be unreachable, ignore
                        }
                    }
                } else {
                    // if that tx id doesn't exist then ignore
                }
            }
        }
    }

    Ok(())
}
