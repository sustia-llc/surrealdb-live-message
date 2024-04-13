# surrealdb-live-message
- demonstrates a light message layer for multi-agent interaction, based on surrealdb live query. A single message record is created for each agent; agents listen for updates to their message record. Message history is stored in a separate table.
## Requirements
* [Rust](https://www.rust-lang.org/tools/install)
* [SurrealDB](https://surrealdb.com/docs/surrealdb/installation/)
## Test
## Terminal 1
```sh
# start surrealdb
surreal start --log debug --user root --pass root memory
```
## Terminal 2
```sh
# run the integration test
cargo test --test integration_test
```
## Run
## Terminal 1
```sh
# start surrealdb
surreal start --log debug --user root --pass root memory
```
## Terminal 2
```sh
# start the app
cargo run
```
## Terminal 3
```sh
# start surrealdb client
surreal sql --user root --pass root --namespace test --database test
# message from bob to alice
update message:alice set from = agent:bob, payload = { Text: { content: 'Hello, Alice!' } }, updated = time::now()
# message from alice to bob
update message:bob set from = agent:alice, payload = { Text: { content: 'Hello, Bob!' } }, updated = time::now()
# check message history
select * from message_history
```
## Documentation

For detailed documentation on SurrealDB, visit [SurrealDB's Documentation](https://surrealdb.com/docs).

## Acknowledgments
* [tokio-graceful-shutdown](https://github.com/Finomnis/tokio-graceful-shutdown)
* [surrealdb](https://github.com/surrealdb/surrealdb)

