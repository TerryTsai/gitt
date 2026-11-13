use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use syntect::parsing::{
    SyntaxDefinition, SyntaxSetBuilder,
    syntax_definition::{ContextReference, MatchOperation, Pattern},
};

fn main() {
    println!("cargo:rerun-if-changed=assets/syntaxes");
    let mut paths = Vec::new();
    grammar_paths(Path::new("assets/syntaxes"), &mut paths);
    paths.sort();
    let mut definitions = BTreeMap::new();
    let mut filenames = BTreeMap::new();
    for path in paths {
        let filename = path.file_stem().unwrap().to_str().unwrap();
        let source = fs::read_to_string(&path).expect("read vendored syntax");
        let syntax = SyntaxDefinition::load_from_str(&source, true, Some(filename))
            .expect("parse vendored syntax");
        let scope = syntax.scope.to_string();
        filenames.insert(filename.to_owned(), scope.clone());
        definitions.insert(scope, syntax);
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    for (pack, root) in [
        ("rust", "source.rust"),
        ("python", "source.python"),
        ("c", "source.c"),
        ("cpp", "source.c++"),
        ("javascript", "source.js"),
        ("json", "source.json"),
        ("css", "source.css"),
        ("html", "text.html.basic"),
        ("shell", "source.shell.bash"),
        ("yaml", "source.yaml"),
        ("markdown", "text.html.markdown"),
        ("sql", "source.sql"),
    ] {
        let mut included = BTreeSet::new();
        include(root, &definitions, &filenames, &mut included);
        let mut builder = SyntaxSetBuilder::new();
        builder.add_plain_text_syntax();
        for scope in &included {
            builder.add(definitions[scope].clone());
        }
        let syntaxes = builder.build();
        let unresolved = syntaxes.find_unlinked_contexts();
        assert!(
            unresolved.is_empty(),
            "Unresolved syntax references in {pack}: {unresolved:?}"
        );
        syntect::dumps::dump_to_uncompressed_file(
            &syntaxes,
            output.join(format!("{pack}.packdump")),
        )
        .expect("write embedded syntax pack");
    }
}

fn grammar_paths(directory: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read syntax directory") {
        let path = entry.expect("read syntax entry").path();
        if path.is_dir() {
            grammar_paths(&path, paths);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "sublime-syntax")
        {
            paths.push(path);
        }
    }
}

fn include(
    scope: &str,
    definitions: &BTreeMap<String, SyntaxDefinition>,
    filenames: &BTreeMap<String, String>,
    included: &mut BTreeSet<String>,
) {
    if !included.insert(scope.to_owned()) {
        return;
    }
    let syntax = definitions
        .get(scope)
        .unwrap_or_else(|| panic!("Missing syntax dependency {scope}"));
    for context in syntax.contexts.values() {
        for pattern in &context.patterns {
            match pattern {
                Pattern::Include(reference) => {
                    dependency(reference, definitions, filenames, included)
                }
                Pattern::Match(pattern) => {
                    if let Some(reference) = &pattern.with_prototype {
                        dependency(reference, definitions, filenames, included);
                    }
                    if let MatchOperation::Push(references) | MatchOperation::Set(references) =
                        &pattern.operation
                    {
                        for reference in references {
                            dependency(reference, definitions, filenames, included);
                        }
                    }
                }
            }
        }
    }
}

fn dependency(
    reference: &ContextReference,
    definitions: &BTreeMap<String, SyntaxDefinition>,
    filenames: &BTreeMap<String, String>,
    included: &mut BTreeSet<String>,
) {
    match reference {
        ContextReference::ByScope {
            scope, with_escape, ..
        } => {
            let scope = scope.to_string();
            if !with_escape || definitions.contains_key(&scope) {
                include(&scope, definitions, filenames, included);
            }
        }
        ContextReference::File {
            name, with_escape, ..
        } => {
            if let Some(scope) = filenames.get(name) {
                include(scope, definitions, filenames, included);
            } else {
                assert!(*with_escape, "Missing syntax file dependency {name}");
            }
        }
        _ => {}
    }
}
