use std::fmt;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

macro_rules! identity {
    ($name:ident, $check:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub(crate) struct $name(pub(crate) String);
        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                ensure!(($check)(&value), "invalid {}: {value}", stringify!($name));
                Ok(Self(value))
            }
        }
        impl TryFrom<String> for $name {
            type Error = anyhow::Error;
            fn try_from(value: String) -> Result<Self> {
                Self::new(value)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

identity!(CommitId, |v: &str| matches!(v.len(), 40 | 64)
    && v.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
identity!(JobId, slug);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn retained_identities_reject_path_traversal() {
        assert!(serde_json::from_str::<JobId>("\"../foreign\"").is_err());
        assert!(CommitId::new("main").is_err());
        assert!(JobId::new("").is_err());
        assert!(CommitId::new("a".repeat(40)).is_ok());
        assert!(CommitId::new("A".repeat(40)).is_err());
    }
}
