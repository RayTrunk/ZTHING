//! File name index and search for ZTHING. Free of Windows dependencies.

mod index;
mod query;

pub use index::{ATTRIBUTE_DIRECTORY, Index, IndexError};
pub use query::{Now, Query, QueryError};
