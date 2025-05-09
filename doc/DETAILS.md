# Description

While [Neutrino](https://github.com/lightninglabs/neutrino/blob/master) is the standard SPV for [LND](https://github.com/lightningnetwork/lnd), integrations with existing Rust clients for [LDK](https://github.com/lightningdevkit) and [BDK](https://github.com/bitcoindevkit) haven't come to fruition. The [Nakamoto](https://github.com/cloudhead/nakamoto) project is complete with some very modular, elegant programming, but the lead maintainer has other projects to focus on. [Murmel](https://github.com/rust-bitcoin/murmel) is yet another light client in Rust, but the last commit was 4 years ago at the time of writing. The Rust community of crates has evolved quickly in terms of asynchronous frameworks and runtime executors. Like the [LDK node](https://github.com/lightningdevkit/ldk-node?tab=readme-ov-file) project, this project leverages the use of `tokio`. By leveraging how futures and executors have developed over the years, the hope is a light client in Rust should be significantly easier to maintain.

This document outlines some miscellaneous information about the project and some recommendations for implementation. The goal for Kyoto is to run on mobile devices with a low-enough memory footprint to allow users to interact with Lightning Network applications as well. To that end, some assumptions and design goals were made to cater to low-resource devices. Neutrino and LND are the current standard of server-style, SPV Lightning Node implementation, so Kyoto is generally compliment and not a substitution.

Some lower-level architecture decisions are documented in a blog series over at [blog.yonson.dev](https://blog.yonson.dev/series/kyoto/).

# Scope

### Functional Goals

- [x] Provide an archival index for blocks and transactions related to a set of `scriptPubKey`, presumably because the user is interested in transactions with these scripts involved.
- [x] Provide an interface to the P2P network, particularly to allow for new transaction broadcasting. Messages will be encrypted if remote nodes advertise support for BIP324. Kyoto also offers experimental support for Tor routing.
- [x] Provide a testing ground for experimentation and research into the Bitcoin P2P network. Particularly in tradeoffs between finding reliable peers, trying new peers, and eclipse attacks.
- [x] Provide rudimentary blockchain data, like the height of the chain, the "chainwork", the `CompactTarget` of the last block, etc.

### Out of Scope

- Any wallet functionality beyond indexing transactions. This includes balances, transaction construction, etc. Bitcoin wallets are complex for a number of reasons, and additional functionality within this scope would detract from other improvements.

# Recommendations

While Kyoto is configurable, there are tradeoffs to each configuration, and some may be better than others. The main advantage to using a light client is increased privacy, but the hope is Kyoto may even lead to a better user experience than standard server-client setups.

### Privacy

Under the assumption that only a few connections should be maintained, broadcasting transactions in a privacy-focused way presents a challenge. Reliable, seeded peers speed up the syncing process, but broadcasting transactions to seeded peers does not offer a significant privacy benefit compared to simply using a server. As such, it is recommended to use a seeded peer, but to allow connections for one or more random peers found by network gossip. When broadcasting a transaction, you may then broadcast your transaction to a random peer. When reaching out to nodes, the Kyoto version message does not contain your users' IP addresses, only _127.0.0.1_, so packet association by nodes you connect is made harder. When downloading blocks, requests are always made to random peers, so scanning for outputs associated with your scripts has a great anonymity set.

### Runtime

Kyoto exposes two components, `node` and `client`, which have different runtime requirements.

1. **`Node`** Requires a tokio runtime since it internally uses the `tokio::task::spawn` hook. So the `node.run()` method must run within a tokio runtime context. The standard multithread runtime is recommended for best performance.

```rust
tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .unwrap()
    .block_on(async move {
        node.run().await;
    })
```

2. **`Client`** Provides async functions that must be executed within some async runtime, but it doesn't rely on tokio-specific features like `tokio::task::spawn`. This means the client can be used with any async runtime (tokio, async-std, etc.) or even a minimal future executor.

### Hosting a Full Archival Node

From an analysis of a `peers.dat`, roughly 20% of nodes signaled for compact block filters support in their service flags. Oftentimes, Kyoto may churn through peers gossiped on the peer to peer network, and this may lead for a bad user experience. Seeded peers help Kyoto sync faster, but some seeds are better than others. Because compact block filters are a database index, they may be stored on an SSD, HD, or external drive. To host a node for end users, it is strongly recommended to use a machine with an SSD.

# Usage Statistics

Leveraging the [Bitcoin Dev Kit's FFI project](https://github.com/bitcoindevkit/bdk-ffi), an [example Swift app](https://github.com/rustaceanrob/BDKKyotoExampleApp) was built and ran on an iPhone 15. The application recovered a wallet from block height 830,000 to roughly block height 895,000. This was done three times, relying on DNS to bootstrap the initial peers each time.

#### Network

| Received |
| -------- |
| 1.5 GB   |

#### Energy Impact

| Battery usage    |
| ---------------- |
| 32.9% "overhead" |
| 30.4% CPU        |
| 36.5% Network    |

#### Memory

| Maximum | Start |
| ------- | ----- |
| 73.5 MB | 32 MB |

#### Disk

| Reads | Writes |
| ----- | ------ |
| 29 MB | 35 MB  |

#### CPU

| Peer 1 | Peer 2 | Peer 3 |
| ------ | ------ | ------ |
| 100%   | 20-50% | 10-50% |

#### Speed

| Peer 1    | Peer 2    | Peer 3     |
| --------- | --------- | ---------- |
| 5-10 MB/s |  < 2 MB/s | 0.5-5 MB/s |
| 76 sec    | 427 sec   | 950 sec    |

# Implementation

This section details what behavior to expect when using Kyoto, and why such decisions were made.

## Peer Selection

Kyoto will first connect to all of the configured peers to maintain the connection requirement, and will use peers gleaned from the peer-to-peer gossip thereafter. If no peers are configured when building the node, and no peers are in the database, Kyoto will resort to DNS. Kyoto will not select a peer of the same netgroup (`/16`) as a previously connected peer. When selecting a new peer from the database, a random preference will be selected between a "new" peer and a peer that has been "tried." Rational is derived from [this research](https://www.ethanheilman.com/p/eclipse/index.html)

## Block Headers and Storage

Kyoto expects users to adopt some form of persistence between sessions when it comes to block header data. Reason being, Kyoto emits block headers that have been reorganized back to the client in such an event. To do so, in a rare but potential circumstance where the client has shut down on a stale tip, one that is reorganized in the future, Kyoto may use the header persistence to load the older chain into memory.

## Filters

Block filters for a full block may be 300-400 bytes, and may be needless overhead if scripts are revealed "into the future" for the underlying wallet. Full filters are checked for matches as they are downloaded, but are discarded thereafter. As a result, if the user adds scripts that are believe to be included in blocks _in the past_, Kyoto will have to redownload the filters. But if the wallet has up to date information, a revealing a new script is guaranteed to have not been used. This memory tradeoff was deemed worthwhile, as it is expected rescanning will only occur for recovery scenarios.

## Structure

* `chain`: Contains all logic for syncing block headers, filter headers, filters, parsing blocks. Also contains preset checkpoints for Signet, Regtest, and Bitcoin networks. Notable files: `chain.rs`, `graph.rs`
* `db`: Defines how data must be persisted with `traits.rs`, and includes some opinionated defaults for database components.
* `network`: Opens and closes connections, handles encryption and decryption of messages, generates messages, parses messages, times message response times, performs DNS lookups. Notable files: `peer.rs`, `reader.rs`, `parsers.rs`, `outbound_messages.rs`

![Layout](https://github.com/user-attachments/assets/21280bb4-aa88-4e11-9223-aed35a885e99)
