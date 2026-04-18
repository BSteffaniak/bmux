//! Semantic validation for a parsed BPDL schema.
//!
//! Verifies that:
//! - Type names are unique within an interface.
//! - All `Named` type references resolve to a declared type.
//! - An interface declares at most one `events <type>`.
//! - The plugin id is non-empty.

use std::collections::BTreeSet;

use crate::{
    Error,
    ast::{Interface, InterfaceItem, Schema, TypeRef},
};

/// Validate a parsed BPDL schema.
///
/// # Errors
///
/// Returns [`Error::Validate`] if the schema violates any semantic rule:
/// duplicate type names, duplicate variant/enum cases, unresolved type
/// references, multiple `events` declarations, or empty plugin id.
pub fn validate(schema: &Schema) -> Result<(), Error> {
    if schema.plugin.plugin_id.trim().is_empty() {
        return Err(Error::Validate {
            message: "plugin id must not be empty".to_string(),
        });
    }
    for iface in &schema.interfaces {
        validate_interface(iface)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_interface(iface: &Interface) -> Result<(), Error> {
    let mut type_names: BTreeSet<String> = BTreeSet::new();
    let mut op_names: BTreeSet<String> = BTreeSet::new();
    let mut events_declared = false;

    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => {
                if !type_names.insert(r.name.clone()) {
                    return Err(Error::Validate {
                        message: format!(
                            "duplicate type `{}` in interface `{}`",
                            r.name, iface.name
                        ),
                    });
                }
            }
            InterfaceItem::Variant(v) => {
                if !type_names.insert(v.name.clone()) {
                    return Err(Error::Validate {
                        message: format!(
                            "duplicate type `{}` in interface `{}`",
                            v.name, iface.name
                        ),
                    });
                }
                let mut case_names = BTreeSet::new();
                for c in &v.cases {
                    if !case_names.insert(c.name.clone()) {
                        return Err(Error::Validate {
                            message: format!("duplicate case `{}` in variant `{}`", c.name, v.name),
                        });
                    }
                }
            }
            InterfaceItem::Enum(e) => {
                if !type_names.insert(e.name.clone()) {
                    return Err(Error::Validate {
                        message: format!(
                            "duplicate type `{}` in interface `{}`",
                            e.name, iface.name
                        ),
                    });
                }
                let mut case_names = BTreeSet::new();
                for c in &e.cases {
                    if !case_names.insert(c.name.clone()) {
                        return Err(Error::Validate {
                            message: format!("duplicate case `{}` in enum `{}`", c.name, e.name),
                        });
                    }
                }
            }
            InterfaceItem::Query(op) | InterfaceItem::Command(op) => {
                if !op_names.insert(op.name.clone()) {
                    return Err(Error::Validate {
                        message: format!(
                            "duplicate operation `{}` in interface `{}`",
                            op.name, iface.name
                        ),
                    });
                }
            }
            InterfaceItem::Events(_) => {
                if events_declared {
                    return Err(Error::Validate {
                        message: format!(
                            "interface `{}` declares `events` more than once",
                            iface.name
                        ),
                    });
                }
                events_declared = true;
            }
        }
    }

    // Validate all type references resolve.
    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => {
                for f in &r.fields {
                    check_type(&f.ty, &type_names, &iface.name)?;
                }
            }
            InterfaceItem::Variant(v) => {
                for c in &v.cases {
                    for f in &c.payload {
                        check_type(&f.ty, &type_names, &iface.name)?;
                    }
                }
            }
            InterfaceItem::Enum(_) => {}
            InterfaceItem::Query(op) | InterfaceItem::Command(op) => {
                for p in &op.params {
                    check_type(&p.ty, &type_names, &iface.name)?;
                }
                check_type(&op.returns, &type_names, &iface.name)?;
            }
            InterfaceItem::Events(ty) => {
                check_type(ty, &type_names, &iface.name)?;
            }
        }
    }

    Ok(())
}

fn check_type(ty: &TypeRef, known: &BTreeSet<String>, iface_name: &str) -> Result<(), Error> {
    match ty {
        TypeRef::Primitive(_) | TypeRef::Unit => Ok(()),
        TypeRef::Named(name) => {
            if known.contains(name) {
                Ok(())
            } else {
                Err(Error::Validate {
                    message: format!(
                        "unknown type `{name}` referenced in interface `{iface_name}`"
                    ),
                })
            }
        }
        TypeRef::Option(inner) | TypeRef::List(inner) => check_type(inner, known, iface_name),
        TypeRef::Result(a, b) => {
            check_type(a, known, iface_name)?;
            check_type(b, known, iface_name)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Error, compile};

    #[test]
    fn accepts_valid_schema() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record r { id: uuid }\n\
                     query q() -> r;\n\
                   }";
        let _ = compile(src).expect("valid");
    }

    #[test]
    fn rejects_duplicate_type() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record r { a: u32 }\n\
                     record r { b: u32 }\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_unknown_type_reference() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     query q() -> missing;\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_duplicate_events_declaration() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record e { kind: u32 }\n\
                     events e;\n\
                     events e;\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }
}
