//! BPDL language constructs added with the canonical contract IR: parsing,
//! validation, lowering, identity and WIT validity.

use bmux_plugin_schema::ir::{
    self, FunctionKind, Item, MethodKind, PackageSet, RefScope, ServiceKind, Type, TypeDefKind,
    WorldItem,
};

const FULL: &str = r#"
/// Files exposed by an example host.
plugin acme.files version 3;

import clock = acme.clock;

capability READ = acme.files.read;
capability WRITE = acme.files.write;

/// File access.
@capability(READ)
@since(1)
interface files {
    /// Permission bits.
    flags mode {
        /// May read.
        read,
        write,
        exec,
    }

    /// An open file handle.
    resource file {
        constructor(path: string);
        query size() -> u64;
        command write(data: bytes) -> result<unit, string>;
        static query exists(path: string) -> bool;
    }

    record stat {
        /// Size and an initial byte.
        pair: tuple<u64, char>,
        mode: mode,
        modified: clock.instant,
    }

    @deprecated(2)
    @since(1)
    query stat(f: borrow<file>) -> stat;
    @unstable(fast-path)
    query open(path: string) -> own<file>;
    query lines(path: string) -> stream<string>;
    query digest(path: string) -> future<bytes>;
}

@capability(WRITE)
interface edits {
    command touch(path: string) -> unit emits files.stat;
}

interface changes {
    @state events files.stat;
}

/// What the files plugin serves.
world files-plugin {
    provide files;
    provide edits;
    provide changes capability acme.files.read;
    provide opaque legacy-files@2: command capability acme.files.write;
    provide capability acme.files.admin;
    require clock.clock;
    require optional capability acme.storage;
}

/// Print a file.
command cat(path: string, lines: u32?) title "Print a file";

extension-point file-preview version 1 {
    data { id: string, order: u32, label: string? }
    interface {
        query render(width: u16) -> list<string>;
    }
}

contribute file-preview@1 { id: "text", order: 10 }
"#;

const CLOCK: &str = "plugin acme.clock version 1;
capability READ = acme.clock.read;
interface types { record instant { seconds: u64 } }
@capability(READ) interface clock { query now() -> types.instant; }";

fn compile_full() -> (ir::Package, PackageSet) {
    let clock = bmux_plugin_schema::compile(CLOCK).expect("clock schema");
    let mut imports = std::collections::BTreeMap::new();
    imports.insert("clock".to_string(), clock.clone());
    let schema =
        bmux_plugin_schema::compile_with_imports(FULL, &imports).expect("full schema compiles");
    let mut set = PackageSet::new();
    set.insert(ir::lower(&clock));
    (ir::lower(&schema), set)
}

#[test]
fn new_constructs_lower_into_the_ir() {
    let (package, _) = compile_full();
    assert_eq!(package.docs, ["Files exposed by an example host."]);
    let files = &package.interfaces[0];
    assert_eq!(files.docs, ["File access."]);
    assert_eq!(files.stability.since, Some(1));
    let Some(Item::Type(mode)) = files.items.first() else {
        panic!("flags first");
    };
    let TypeDefKind::Flags { flags } = &mode.kind else {
        panic!("flags kind");
    };
    assert_eq!(flags.len(), 3);
    assert_eq!(flags[0].docs, ["May read."]);
    let TypeDefKind::Resource { methods } = &files.type_def("file").expect("resource").kind else {
        panic!("resource kind");
    };
    assert_eq!(
        methods.iter().map(|m| m.kind).collect::<Vec<_>>(),
        [
            MethodKind::Constructor,
            MethodKind::Query,
            MethodKind::Command,
            MethodKind::StaticQuery
        ]
    );
    let stat = files
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(function) if function.name == "stat" => Some(function),
            _ => None,
        })
        .expect("stat query");
    assert_eq!(stat.kind, FunctionKind::Query);
    assert_eq!(stat.stability.deprecated, Some(2));
    assert!(
        matches!(&stat.params[0].ty, Type::Borrow(reference) if reference.scope == RefScope::Local)
    );
    let TypeDefKind::Record { fields } = &files.type_def("stat").expect("record").kind else {
        panic!("record");
    };
    assert!(matches!(&fields[0].ty, Type::Tuple(items) if items.len() == 2));
    assert!(
        matches!(&fields[2].ty, Type::Ref(reference) if reference.scope == RefScope::Import("clock".into()))
    );

    let world = &package.worlds[0];
    assert_eq!(world.items.len(), 7);
    assert!(matches!(
        &world.items[3],
        WorldItem::ProvideOpaque {
            kind: ServiceKind::Command,
            version: Some(2),
            ..
        }
    ));
    assert_eq!(package.commands[0].title.as_deref(), Some("Print a file"));
    assert_eq!(
        package.extension_points[0].id(&package.id),
        "acme.files.file-preview/v1"
    );
    assert_eq!(package.contributions[0].values.len(), 2);
}

#[test]
fn new_constructs_have_identity_and_docs_or_gates_do_not() {
    let (package, set) = compile_full();
    for iface in &package.interfaces {
        assert_eq!(
            bmux_plugin_schema::canonical::interface_identity(&set, &package, iface).len(),
            32,
            "{}",
            iface.name
        );
    }
    let point = &package.extension_points[0];
    assert_eq!(
        bmux_plugin_schema::canonical::extension_point_identity(&set, &package, point).len(),
        32
    );

    let identity = |source: &str| {
        let clock = bmux_plugin_schema::compile(CLOCK).unwrap();
        let schema = bmux_plugin_schema::compile(source).unwrap();
        let mut set = PackageSet::new();
        set.insert(ir::lower(&clock));
        let package = ir::lower(&schema);
        bmux_plugin_schema::canonical::interface_identity(&set, &package, &package.interfaces[0])
    };
    let base = identity(FULL);
    // Documentation and stability gates are metadata, not wire contract.
    assert_eq!(base, identity(&FULL.replace("/// File access.\n", "")));
    assert_eq!(base, identity(&FULL.replace("@unstable(fast-path)\n", "")));
    // Wire changes in new constructs change identity.
    assert_ne!(base, identity(&FULL.replace("exec,", "exec, sticky,")));
    assert_ne!(
        base,
        identity(&FULL.replace("tuple<u64, char>", "tuple<u64, u8>"))
    );
    assert_ne!(
        base,
        identity(&FULL.replace("stream<string>", "list<string>"))
    );
    assert_ne!(
        base,
        identity(&FULL.replace("query size()", "command size()"))
    );
}

#[test]
fn full_schema_lowers_to_valid_wit() {
    let (package, set) = compile_full();
    let mut resolve = wit_parser::Resolve::new();
    let clock = set.get("acme.clock").expect("clock");
    resolve
        .push_source(
            "clock.wit",
            &bmux_plugin_schema::wit::emit(&set, clock).unwrap(),
        )
        .expect("clock WIT");
    let source = bmux_plugin_schema::wit::emit(&set, &package).expect("files WIT");
    resolve
        .push_source("files.wit", &source)
        .unwrap_or_else(|error| panic!("{error:?}\n{source}"));
    for expected in [
        "flags mode",
        "resource file",
        "constructor(path: string);",
        "exists: static func(path: string) -> bool;",
        "stat-fn: func(f: borrow<file>) -> stat;",
        "@unstable(feature = fast-path)",
        "@deprecated(version = 2.0.0)",
        "lines: func(path: string) -> stream<string>;",
        "touch: func(path: string) -> tuple<stream<stat>, future>;",
        "export files;",
        "import acme:clock/clock@1.0.0;",
        "interface file-preview-v1",
        "interface plugin-commands",
    ] {
        assert!(
            source.contains(expected),
            "missing `{expected}` in\n{source}"
        );
    }
}

fn rejected(source: &str) -> String {
    bmux_plugin_schema::compile(source)
        .expect_err("schema must be rejected")
        .to_string()
}

#[test]
fn invalid_new_constructs_are_rejected() {
    let header = "plugin p.q version 2; capability R = p.read;";
    for (body, needle) in [
        (
            "@capability(R) interface i { record r { n: u32 } query get(h: own<r>) -> unit; }",
            "must name a resource",
        ),
        (
            "@capability(R) interface i { resource f { } query get() -> borrow<f>; }",
            "only allowed in parameters",
        ),
        ("interface i { flags f { a, a } }", "duplicate flag"),
        (
            "@since(3) interface i { record r { n: u32 } }",
            "package version",
        ),
        (
            "@unstable(x) @since(1) interface i { record r { n: u32 } }",
            "both `@unstable`",
        ),
        (
            "@deprecated(1) interface i { record r { n: u32 } }",
            "requires an earlier or equal `@since`",
        ),
        ("world w { provide missing; }", "unknown interface"),
        (
            "interface t { record r { n: u32 } } world w { provide t; }",
            "types-only",
        ),
        (
            "interface e { record r { n: u32 } events r; } world w { provide e; }",
            "must name the `capability <id>`",
        ),
        (
            "@capability(R) interface s { query get() -> u32; } world w { provide s capability p.other; }",
            "must not restate",
        ),
        ("command run(data: bytes);", "scalar"),
        (
            "extension-point x version 1 { data { id: string } } contribute x@1 { }",
            "missing required field",
        ),
        (
            "extension-point x version 1 { data { id: string } } contribute x@1 { id: 3 }",
            "does not match",
        ),
    ] {
        let message = rejected(&format!("{header} {body}"));
        assert!(message.contains(needle), "`{body}`: {message}");
    }
}

#[test]
fn contextual_keywords_remain_usable_as_names() {
    let schema = bmux_plugin_schema::compile(
        "plugin p.q version 1; capability R = p.read;
         interface world { record stream { future: u32, tuple: u32 } }
         @capability(R) interface s { query resource(flags: world.stream) -> world.stream; }",
    )
    .expect("contextual words are names without `<`");
    assert_eq!(schema.interfaces[0].name, "world");
}
