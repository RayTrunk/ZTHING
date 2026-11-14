//! File name index and search for ZTHING. Free of Windows dependencies.

mod index;
mod query;

pub use index::{ATTRIBUTE_DIRECTORY, Index, IndexError, SortKey};
pub use query::{Now, Query, QueryError};
