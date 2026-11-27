# Contributing to Kyoto

The Kyoto project operates an open contributor model where anyone is welcome to
contribute towards development in the form of peer review, documentation,
testing and patches.

Anyone is invited to contribute without regard to technical experience,
"expertise", OSS experience, age, or other concern. However, the development of
protocols for cryptocurrencies demands a high-level of rigor, adversarial thinking, thorough
testing and risk-minimization.
Any bug may cost users real money. That being said, contributions are welcome.

## Communications Channels

All communication happens on GitHub, in the form of issues, pull requests, milestones, etc.

## Useful Prerequisites

This is a description of the background knowledge that is useful for Kyoto development. There are no requirements to contribute, and this information is meant as a guide to be an effective contributor.

Kyoto relies on the `tokio` asynchronous runtime and adopts many of the tools from the framework. Namely, developers should feel comfortable with [channels](https://tokio.rs/tokio/tutorial/channels) and concurrent access of shared resources behind an `Arc<Mutex<T>>`. Furthermore, Kyoto communicates with the peer-to-peer network via TCP connections. General knowledge of how TCP, Socks5, DNS, and other internet protocols is not essential but certainly helpful. Following the `tokio` [tutorial](https://tokio.rs/tokio/tutorial) is a good place to start.

Performance is central to developing a Bitcoin light client. Having a strong grasp of how implementation choices impact the use and access of memory is very valuable. This includes how memory can be exploited by adversaries.

Of course, a Bitcoin light client involves many domain-specific topics. If the Bitcoin protocol is unfamiliar, [Mastering Bitcoin](https://github.com/bitcoinbook/bitcoinbook) and [Learn me a Bitcoin](https://learnmeabitcoin.com/) are good resources to start. For this implementation in particular, background knowledge on [BIP157](https://github.com/bitcoin/bips/blob/master/bip-0157.mediawiki) and [BIP158](https://github.com/bitcoin/bips/blob/master/bip-0158.mediawiki) is incredibly useful to understand the node's internal state-machine, scope, and expected behavior.

## Contribution Workflow

The codebase is maintained using the "contributor workflow" where everyone
without exception contributes patch proposals using "pull requests". This
facilitates social contribution, easy testing and peer review.

To contribute a patch, the workflow is as follows:

1. Fork Repository
2. Create topic branch
3. Commit patches

In general commits should be atomic and diffs should be easy to read.
For this reason do not mix any formatting fixes or code moves with actual code
changes. Further, each commit, individually, should compile and pass tests, in
order to ensure git bisect and other automated tools function properly.

When adding a new feature, thought must be given to the long term technical
debt.
Every new feature should be covered by functional tests where possible.

When refactoring, structure your PR to make it easy to review and don't
hesitate to split it into multiple small, focused PRs.

The Minimal Supported Rust Version is **1.63.0** (enforced by our CI).

Commits should cover both the issue fixed and the solution's rationale.
These [guidelines](https://chris.beams.io/posts/git-commit/) should be kept in mind. Commit messages should follow the ["Conventional Commits 1.0.0"](https://www.conventionalcommits.org/en/v1.0.0/) to make commit histories easier to read by humans and automated tools.

Commits should be signed with GPG, SSH, or S/MIME, and this will be enforced by GitHub
when merging pull requests. Read more about [signing commits](https://docs.github.com/en/authentication/managing-commit-signature-verification/signing-commits).

To facilitate communication with other contributors, the project is making use
of GitHub's "assignee" field. First check that no one is assigned and then
comment suggesting that you're working on it. If someone is already assigned,
don't hesitate to ask if the assigned party or previous commenter are still
working on it if it has been awhile.

## Coding Conventions

Use `cargo fmt` with the default settings to format code before committing.
Running `cargo clippy --all-targets` should also
pass with no lints. This is also enforced by the CI.

Use the [Rust API guidelines](https://rust-lang.github.io/api-guidelines/about.html) whenever applicable.

The use of any `unsafe` code blocks is completely forbidden.

Any use of `clone` will be scrutinized, and use of `unwrap` or `expect` is generally forbidden.

Kyoto is dependency-adverse. No additional crates will be added to the Kyoto core library, and any additional
dependencies should be added behind a non-default feature.

## Deprecation policy

Where possible, breaking existing APIs should be avoided. Instead, add new APIs and
use [`#[deprecated]`](https://GitHub.com/rust-lang/rfcs/blob/master/text/1270-deprecation.md)
to discourage use of the old one.

Deprecated APIs are typically maintained for one release cycle. In other words, an
API that has been deprecated with the 0.10 release can be expected to be removed in the
0.11 release. This allows for smoother upgrades without incurring too much technical
debt inside this library.

If you deprecated an API as part of a contribution, we encourage you to "own" that API
and send a follow-up to remove it as part of the next release cycle.

## Peer review

Anyone may participate in peer review which is expressed by comments in the
pull request. Typically reviewers will review the code for obvious errors, as
well as test out the patch set and opine on the technical merits of the patch.
PR should be reviewed first on the conceptual level before focusing on code
style or grammar fixes.

## Security

Security is a high priority of Kyoto; disclosure of security vulnerabilities helps
prevent user loss of funds. In the discovery of a security vulnerability, please
create an issue on GitHub.

## Testing

Related to the security aspect, Kyoto developers take testing very seriously.
Due to the modular nature of the project, writing new functional tests is easy
and good test coverage of the codebase is an important goal.

### Integration Testing

The preferred workflow is by using `just`. If you do not have `just` installed, check out the [installation page](https://just.systems/man/en/chapter_4.html).

To run the unit tests and doc tests:

```
just test
```

To sync with a live Signet node:

```
just test sync
```

And to run scenarios against your `bitcoind` instance, set a `BITCOIND_EXE` environment variable to the path to `bitcoind`:

```
export BITCOIND_EXE = "/usr/path/to/bitcoind/"
```

You may want to add this to your bash or zsh profile.

To run the `bitcoind` tests:

```
just test integration
```

If you do not have `bitcoind` installed, you may simply run `just test integration` and it will be installed for you in the `build` folder.

