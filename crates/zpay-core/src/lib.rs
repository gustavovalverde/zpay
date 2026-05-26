//! Domain types and protocol-neutral payment lifecycle for zpay.
//!
//! See the [public interfaces spine][spine] for the vocabulary every type
//! here defers to.
//!
//! This crate is library-only. It carries no async runtime, no HTTP server,
//! no database driver. The wire adapters ([`zpay-x402`][x402] and
//! [`zpay-mpp`][mpp]) and the runtime ([`zpay-runtime`][runtime]) compose
//! these primitives.
//!
//! [spine]: https://github.com/gustavovalverde/zpay/blob/main/docs/architecture/public-interfaces.md
//! [x402]: https://docs.rs/zpay-x402
//! [mpp]: https://docs.rs/zpay-mpp
//! [runtime]: https://docs.rs/zpay-runtime

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod broadcast;
pub mod capability;
pub mod error;
pub mod oracle;
pub mod prepare;
pub mod settle;
pub mod types;
pub mod verify;
