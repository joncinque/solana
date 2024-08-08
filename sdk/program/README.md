<p align="center">
  <a href="https://solana.com">
    <img alt="Solana" src="https://i.imgur.com/IKyzQ6T.png" width="250" />
  </a>
</p>

# Solana Program

Use the Solana Program Crate to write on-chain programs in Rust.  If writing client-side applications, use the [Solana SDK Crate](https://crates.io/crates/solana-sdk) instead.

More information about Solana is available in the [Solana documentation](https://solana.com/docs).

[Solana Program Library](https://github.com/solana-labs/solana-program-library) provides examples of how to use this crate.

Still have questions?  Ask us on [Stack Exchange](https://sola.na/sse)

## Concepts for the Program SDK

* Respect semantic versioning
* On-chain program developers come first
* Success should be possible quickly and easily
* Sensible defaults but allow customization
* Keep it simple to get going, but expose more complicated stuff little by little
* Secure
* Minimal dependencies
* Kitchen sinks only re-export functionality
* Default to new crates or files to bundle functionality
* Only if new functionality can't be done in a new crate or file, use features
* Easy to publish
* Easy for Agave devs to make changes
* Play nice with code generation (Kinobi, Anchor, etc)

## Project Plan Phase 1: solana_program updates

### Break it up: if it can reasonably be used independently, it gets its own crate

* Add crates required for program clients:
  * borsh
  * clock
  * example-mocks
  * hash
  * instruction
  * message
  * program-error
  * pubkey
  * serde-varint
  * sysvar
* Those crates allow us to make program client crates:
  * address lookup table
  * bpf loader upgradeable
  * feature
  * loader v4
  * stake
  * system
  * vote
* Next, focus on crates for writing programs:
  * account-info
  * entrypoint
  * program-pack
  * program-syscalls
  * stable-layout
* Finally, do all the rest:
  * native-token
  * serialize-utils
  * wasm
* Deprecate any bits that make sense
* `solana_program` becomes a crate with only re-exports and deprecated code

### Add crate features

* Make serde / borsh / bincode optional with crate features
* Consider adding an "unstable" feature for new code that could break

### Update CI

* Move away from Buildkite, use only GitHub Actions
* Add publish step to generate notes with git-cliff
* Add downstream testing of template programs and new programs

### Move solana-program to a new repo

Note: this does not depend on the breakup of the crates, but probably makes more sense to do after

* Add a warning to Agave anytime a PR is opened modifying files that will be moved
* Create the monorepo
* Add scripts and instructions to agave and SPL to patch local solana-program crates

## Open Questions

* Does it make sense to make a new crate for every sysvar? Clock is useful separately, but what about rent, slot hashes, etc?
* Does it make sense to move ed25519 / keccak / secp256k1 to dedicated curve crates?

## Project Plan Phase 2: solana_sdk updates

Slightly different strategy: solana_sdk contains some modules used by downstream
devs and some used only by Agave, so we might need to keep this package in the
monorepo while deprecating the Agave-specific crates.
