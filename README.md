# surrealdb-live-message
- demonstrates a light message layer for multi-agent interaction, based on surrealdb live query. Message records are sent by establishing a graph relation from sending agent to receiving agent.
## Requirements
* [Rust](https://www.rust-lang.org/tools/install)
* [Docker](https://docs.docker.com/get-docker/)
* [SurrealDB](https://surrealdb.com/docs/surrealdb/installation/) (for client sql queries, see Run below)
## Test
```sh
# run the integration test
cargo test --test integration_test
```
## Run
## Terminal 1
```sh
# start the app
cargo run
# when finished with client below, hit ctrl-c to view the log outputs from the subsystem and listener shutdowns
```
## Terminal 2
```sh
# upgrade to current alpha release
sudo surreal upgrade --version 2.0.0-alpha.10
# start surrealdb client
surreal sql --user root --pass root --namespace test --database test
# message from bob to alice
RELATE agent:bob->message->agent:alice 
	CONTENT {
		created: time::now(),
        	payload: { Text: { content: 'Hello, Alice!' } },
	};
# message from alice to bob
RELATE agent:alice->message->agent:bob 
	CONTENT {
		created: time::now(),
        	payload: { Text: { content: 'Hello, Bob!' } },
	};

# check messages
select * from message
```
## Documentation

For detailed documentation on SurrealDB, visit [SurrealDB's Documentation](https://surrealdb.com/docs).

## Acknowledgments
* [tokio-graceful-shutdown](https://github.com/Finomnis/tokio-graceful-shutdown)
* [surrealDB](https://github.com/surrealdb/surrealdb)

