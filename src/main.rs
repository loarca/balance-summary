use std::{
    collections::{HashMap, HashSet},
    fs::File,
    ops::{Add, AddAssign, Sub, SubAssign},
    path::PathBuf,
};

use clap::Parser;
use csv::{ReaderBuilder, Trim, WriterBuilder};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::{error, instrument, level_filters::LevelFilter, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    Csv(csv::Error),

    #[error("found unknown transaction type: {1}")]
    UnknownTransactionType(TransactionId, String),
    #[error("found transaction with missing amount")]
    MissingAmount(TransactionId),
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
                    return Err(Error::MissingAmount(id));
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
                    return Err(Error::MissingAmount(id));
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
            _ => Err(Error::UnknownTransactionType(id, value.kind)),
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
#[derive(Serialize)]
struct AccountRecord {
    client: u16,
    available: Decimal,
    held: Decimal,
    total: Decimal,
    locked: bool,
}
impl AccountRecord {
    fn new(client_id: ClientId, account: Account) -> Self {
        Self {
            client: client_id.0,
            available: account.available.0,
            held: account.held.0,
            total: account.available.0 + account.held.0,
            locked: account.is_frozen,
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
    fn into_iter(self) -> impl Iterator<Item = (ClientId, Account)> {
        self.0
            .into_iter()
            .enumerate()
            .filter_map(|(id, maybe_account)| {
                maybe_account.map(|account| (ClientId(id as u16), account))
            })
    }
}

/// Account Balance Engine: read transactions from CSV and calculate account balances
#[derive(clap::Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the transactions CSV file
    input: PathBuf,
}

/// 256KB buffer
const BUFFER_SIZE: usize = 256 * 1024;

fn main() -> Result<(), Error> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .pretty()
        .init();

    let cli = Cli::parse();

    // instead of loading the whole file in memory
    // we stream it and deserialize it in chunks
    //
    // this could be replaced with any Read type, like tcp streams or stdin
    let file = File::open(&cli.input).map_err(Error::Io)?;
    let tx_records = ReaderBuilder::new()
        .has_headers(true)
        .delimiter(b',')
        .trim(Trim::All)
        .flexible(true)
        .buffer_capacity(BUFFER_SIZE)
        .from_reader(file)
        .into_deserialize::<TransactionRecord>();

    // used to enforce that transactions are globally unique
    // if we find duplicates those should be dropped
    let mut seen_tx: HashSet<TransactionId> = HashSet::new();
    let mut accounts = AccountsMap::new();

    for record in tx_records {
        let Some(transaction) = record
            .map_err(Error::Csv)
            .and_then(Transaction::try_from)
            .inspect_err(|err| {
                error!(%err, "failed to parse transaction, skipping");
            })
            .ok()
        else {
            continue;
        };

        handle_single_transaction(&mut accounts, &mut seen_tx, transaction);
    }

    let stdout = std::io::stdout().lock();
    let mut csv_writer = WriterBuilder::new()
        .has_headers(true)
        .delimiter(b',')
        .buffer_capacity(BUFFER_SIZE)
        .from_writer(stdout);
    for (client_id, account) in accounts.into_iter() {
        let record = AccountRecord::new(client_id, account);
        csv_writer.serialize(record).map_err(Error::Csv)?;
    }
    csv_writer.flush().map_err(Error::Io)?;

    Ok(())
}

#[instrument(level = "error", skip_all, fields(
    tx_id = transaction.id.0,
    tx_kind = ?transaction.kind,
    client_id = transaction.client_id.0,
))]
fn handle_single_transaction(
    accounts: &mut AccountsMap,
    seen_tx: &mut HashSet<TransactionId>,
    transaction: Transaction,
) {
    if transaction.kind.id_must_be_new() {
        // mark tx ids as seen only for non-dispute transactions
        let is_new_tx_id = seen_tx.insert(transaction.id);
        if !is_new_tx_id {
            // skip duplicate tx ids
            error!("found repeated transaction id for a non-dispute, discarding");
            return;
        }
    }

    let account = accounts.get_or_create(transaction.client_id);
    if account.is_frozen {
        // skip frozen accounts
        error!("account is already frozen");
        return;
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
                error!(
                    available = %account.available.0,
                    "client has not enough funds to withdraw",
                );
                return;
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
                        warn!("funds must be available first in order to file a dispute");
                    }
                }
            } else {
                warn!("unknown transaction id to dispute, ignoring");
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
                        warn!("funds must be held first in order to resolve them");
                    }
                }
            } else {
                warn!("unknown transaction id to resolve, ignoring");
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
                        warn!("funds must be held first in order to charge them back");
                    }
                }
            } else {
                warn!("unknown transaction id to chargeback, ignoring");
            }
        }
    }
}
