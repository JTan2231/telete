//! Usher's schema-one report/check output. Both commands use the same report;
//! check additionally exits unsuccessfully when `incomplete` is nonzero.

use serde::{Deserialize, Serialize};

pub use crate::evidence::{Finding, Issue, Status};
pub use crate::operations::{
    OperationalInventory, OperationalProduct, OperationalUnitDeclaration, inspect_operations,
};
pub use crate::report::{CheckReport, IncompleteProduct, ProductReport, Report, inspect};

/// Fatal command or inventory failure, rather than a membership finding.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ErrorReport {
    pub schema_version: u32,
    pub error: String,
}

impl ErrorReport {
    #[must_use]
    pub fn new(error: String) -> Self {
        Self {
            schema_version: 1,
            error,
        }
    }
}
