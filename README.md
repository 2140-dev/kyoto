<div align="center">
  <h1>Kyoto: Bitcoin Light Client</h1>
  <p>
    <strong>An Implementation of BIP-157/BIP-158</strong>
  </p>

  <p>
    <a href="https://crates.io/crates/kyoto-cbf"><img alt="Crate Info" src="https://img.shields.io/crates/v/kyoto-cbf.svg"/></a>
    <a href="https://github.com/2140-dev/kyoto/blob/master/LICENSE"><img alt="MIT or Apache-2.0 Licensed" src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg"/></a>
    <a href="https://github.com/2140-dev/kyoto/actions?query=workflow%3A%22Build+%26+Test%22"><img alt="CI Status" src="https://github.com/2140-dev/kyoto/workflows/CI/badge.svg"/></a>
    <a href="https://docs.rs/kyoto-cbf"><img alt="API Docs" src="https://img.shields.io/badge/docs.rs-kyoto_cbf-green"/></a>
    <a href="https://blog.rust-lang.org/2022/08/11/Rust-1.63.0.html"><img alt="Rustc Version 1.63.0+" src="https://img.shields.io/badge/rustc-1.63.0%2B-lightgrey.svg"/></a>
  </p>
</div>

## Notice

The development of this crate has been moved to [bip157](https://crates.io/crates/bip157).

## About

Kyoto is aiming to be a simple, memory-conservative, and private Bitcoin client for developers to build wallet applications. To read more about the scope, usage recommendations, and implementation details, see [DETAILS.md](./doc/DETAILS.md).

## Running an example

To run the Signet example, in the root directory:

```
cargo run --example signet
```

Or, with `just`:

```
just example
```

## Getting Started

It is recommended to walk through the [Signet example code](./example/signet.rs). Unlike usual clients that source data from the blockchain, there are two components to the CBF system. There is a "node" that fetches data on behalf of a user, and a "client" that receives data, logs, and warnings from the node. The client may also interact with the node by sending transactions to broadcast or fetching metadata. This crate allows a highly configurable node construction, so your app may optimize for the desired speed, privacy, and preferences.

See the [docs](https://docs.rs/kyoto-cbf) for more details on the `Builder`, `Node`, `Client`, and more.

### BDK

Kyoto integrates well with the Bitcoin Dev Kit (BDK) ecosystem.

* **[Book of BDK - Kyoto Examples](https://bookofbdk.com/cookbook/syncing/kyoto/)**: Learn how to use Kyoto with BDK Wallet through step-by-step examples.
* **[BDK FFI](https://github.com/bitcoindevkit/bdk-ffi)**: Build native Bitcoin experiences across multiple platforms by combining Kyoto with BDK's foreign function interface libraries.
* **[bdk-kyoto Crate](https://github.com/bitcoindevkit/bdk-kyoto)**: A dedicated crate that provides direct integration between BDK and Kyoto for Rust applications.

## Minimum Supported Rust Version (MSRV) Policy

The `kyoto` core library with default features supports an MSRV of Rust 1.63.

## Contributing

Please read [CONTRIBUTING.md](./CONTRIBUTING.md) to get started.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

## License

Licensed under either of

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.
