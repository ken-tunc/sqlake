//! Domain types and driver traits. No database dependencies live here.
//!
//! Everything in this crate is shared between the application layer and the
//! drivers. It deliberately knows nothing about PostgreSQL, BigQuery or the
//! terminal, so that a decision made in one of those places cannot leak into
//! the others.

pub mod capability;
pub mod driver;
pub mod id;
pub mod ident;
pub mod node;
pub mod result;
pub mod value;

pub use capability::{Capabilities, DriverKind, HierarchyLevel, QuoteStyle};
pub use driver::{Driver, DriverError, DriverResult, Session};
pub use id::{ConnId, TabId};
pub use ident::{Ident, QuotedIdent};
pub use node::{NodeKind, NodeRef, RelationKind, TableRef, TreeNode};
pub use result::{Column, PageRequest, ResultSet, Row, Sort, SortDir};
pub use value::Value;
