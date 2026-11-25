# balance-summary

Small exercise to calculate the balances of multiple client accounts.
It reads transactions from CSV and outputs the final balances as CSV as well.

```sh
cargo run -- transactions.csv > accounts.csv
```

Logs go to stderr by default. But if you want to disable them completely then run:

```sh
RUST_LOG=off cargo run -- transactions.csv > accounts.csv
```

## A few assumptions

- Only deposits can be disputed.
- A dispute must come from the same client that made the deposit.
- A dispute can make available funds go negative.
  - For example: deposit, withdraw it all and then dispute and charge back.
- Once an account is locked all transactions are ignored.
- A duplicate transaction id on a deposit or withdrawal is ignored,
  because ids are supposed to be globally unique.
- A resolved deposit can be disputed again.
- Amounts with more than 4 decimal places are truncated.
- Rows that fail to parse are skipped (and logged).

## Samples

`samples/` has one folder per scenario, each one with `input.csv` and `output.csv`.
Every output was built by running the binary on each input.

The `samples/big` example has 1M transactions with 5K clients,
in debug mode it takes around 4.7s with logs disabled on a Mac Studio M1 Max and about 17s with logs enabled.

## Safety and errors

- The program only fatally fails if it can't read the input or write the output.
- Parse errors don't stop the whole process, it just skips the row.
- The only `expect`s are on things that definitely can't fail.
- There's no `unsafe`.

## Efficiency

- The CSV is streamed with a buffered reader (within the csv reader) of 256KB,
  so the file is never fully loaded in memory.
- Since client ids are `u16` accounts are indexed by the client id in a fixed array,
  so there's no hashing for lookups.
- The sample with 1M transactions runs in about 1 second with ~43MB of memory in release mode with logs disabled:

```sh
RUST_LOG=off /usr/bin/time -l cargo run --release -- samples/big/input.csv > samples/big/output.csv
```

(use -v if on linux)

For many TCP streams it's as easy as swapping the file reader for a TCP reader,
so it can be agnostic of the kind of IO.

For concurrency the only requirement would be that transactions should be grouped by accounts
because the order of the transactions matter and the tx id is not guaranteed to be in order of application.
But other than that, this can very easily be turned into async code with `tokio`,
can be updated to use channels or even distributed but only as long as the transactions are grouped by client.

Otherwise the input CSV schema would need to include some sort of timestamp to reconcile transactions against those,
and that's a whole separate problem to solve but would still need a queue per client.
