//! Declarative generator for the `as_db_str` / `from_db_str` pair shared by the
//! crate's persisted enums. Defining both directions from a single
//! variant-to-string table makes it impossible to add a variant to one mapping
//! and silently forget the other.

/// Generate `as_db_str` / `from_db_str` for an enum from one variant table.
///
/// Two forms, matching the two shapes already used in the codebase:
///
/// - `... } else $default` — unknown DB strings decode to `$default`. For `Copy`
///   enums whose `as_db_str` takes `self` and returns `&'static str`.
/// - `... } other = $other` — unknown DB strings are captured by an
///   `$other(String)` variant. For enums whose `as_db_str` borrows `self` and
///   returns `&str`.
#[macro_export]
macro_rules! impl_db_enum {
    ($ty:ident { $($variant:ident => $db:literal),+ $(,)? } else $default:ident) => {
        impl $ty {
            pub fn as_db_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $db,)+
                }
            }

            pub fn from_db_str(s: &str) -> Self {
                match s {
                    $($db => Self::$variant,)+
                    _ => Self::$default,
                }
            }
        }
    };
    ($ty:ident { $($variant:ident => $db:literal),+ $(,)? } other = $other:ident) => {
        impl $ty {
            pub fn as_db_str(&self) -> &str {
                match self {
                    $(Self::$variant => $db,)+
                    Self::$other(s) => s.as_str(),
                }
            }

            pub fn from_db_str(s: &str) -> Self {
                match s {
                    $($db => Self::$variant,)+
                    other => Self::$other(other.to_string()),
                }
            }
        }
    };
}
