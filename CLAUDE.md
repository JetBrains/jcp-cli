# Running ant testing project

- Do not run project directly using `cargo run` it will block stdin, but you can use `cargo run -- --help` or other commands that will terminate deterministically
- Use `cargo check` and `cargo test` to test that projects works

# Rules

- When having a struct for representing JSON payload using serde, use `#[serde(rename = ...)]` to specify field name in JSON and prevent any possible problems when renaming Rust field names
- Do not to change code that is beyond requirements of a current task.
- You can and should put meaningful comments to your code, but do not add comments to code that you didn't wrote or didn't changed in the task scope.
- When adding functionality assess if a new test should be written, if so do it

# Terms

- downlink - communication channel between ACP client (IDE) and this program (adapter)
- uplink - communication channel between this program (adapter) and ACP server (JCP)
- transport – defines how to send a receive messages from uplink/downlink channels
