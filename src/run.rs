//! Run configurations: what can run in a file (`main`, tests) and in the
//! project (cargo, npm scripts, go, make, pytest), as shell commands that
//! the workspace runs in a terminal tab.

use std::path::{Path, PathBuf};

use tree_sitter::{Node, Tree};

use crate::syntax::Language;

/// Something runnable: a configuration in Run…, or a gutter ▶ on `row`.
#[derive(Clone, Debug, PartialEq)]
pub struct RunTarget {
    /// "Run 'main'", "Test 'parses_input'", "npm run dev".
    pub name: String,
    /// A shell command line.
    pub command: String,
    pub cwd: PathBuf,
    /// The line it belongs to, for gutter targets.
    pub row: Option<usize>,
    /// Lines it spans (the function or block), for Run at caret.
    pub rows: std::ops::Range<usize>,
}

/// Quotes for a POSIX shell when needed.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@+%,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn nearest_with(file: &Path, marker: &str) -> Option<PathBuf> {
    file.ancestors().skip(1).find(|d| d.join(marker).is_file()).map(Path::to_path_buf)
}

fn node_text<'a>(node: Node, text: &'a str) -> &'a str {
    &text[node.byte_range()]
}

fn target(name: String, command: String, cwd: &Path, node: Node, header: Node) -> RunTarget {
    RunTarget {
        name,
        command,
        cwd: cwd.to_path_buf(),
        row: Some(header.start_position().row),
        rows: node.start_position().row..node.end_position().row + 1,
    }
}

/// Runnable things in a file, for the gutter and Run at caret.
pub fn file_targets(language: Language, path: &Path, text: &str, tree: &Tree) -> Vec<RunTarget> {
    let root = tree.root_node();
    let mut out = Vec::new();
    match language {
        Language::Rust => rust_targets(path, text, root, &mut out),
        Language::Go => go_targets(path, text, root, &mut out),
        Language::Python => python_targets(path, text, root, &mut out),
        Language::JavaScript | Language::TypeScript | Language::Tsx => js_targets(path, text, root, &mut out),
        _ => {}
    }
    out.sort_by_key(|t| t.row);
    out
}

// ---- Rust ------------------------------------------------------------------------

/// The module path of a source file inside its crate: `src/a/b.rs` → `a::b`.
fn rust_module_path(path: &Path, crate_dir: &Path) -> Vec<String> {
    let Ok(rel) = path.strip_prefix(crate_dir.join("src")) else { return Vec::new() };
    let mut parts: Vec<String> = rel.with_extension("").iter().map(|p| p.to_string_lossy().into_owned()).collect();
    if parts.last().is_some_and(|p| p == "mod" || p == "lib" || p == "main") {
        parts.pop();
    }
    // Binaries in src/bin are their own crates.
    if parts.first().is_some_and(|p| p == "bin") {
        return parts.into_iter().skip(2).collect();
    }
    parts
}

fn rust_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let Some(crate_dir) = nearest_with(path, "Cargo.toml") else { return };
    let rel = path.strip_prefix(&crate_dir).unwrap_or(path);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    // `cargo test` needs to know which target an integration test or binary is.
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let target_flag = if rel_str.starts_with("tests/") {
        format!(" --test {}", sh_quote(&stem))
    } else if rel_str.starts_with("src/bin/") {
        format!(" --bin {}", sh_quote(&stem))
    } else if rel_str.starts_with("examples/") {
        format!(" --example {}", sh_quote(&stem))
    } else {
        String::new()
    };
    let module = rust_module_path(path, &crate_dir);

    fn has_test_attr(node: Node, text: &str) -> bool {
        let mut prev = node.prev_named_sibling();
        while let Some(p) = prev.filter(|p| p.kind() == "attribute_item" || p.kind() == "line_comment") {
            let attr = node_text(p, text);
            if attr.starts_with("#[test") || attr.contains("::test") || attr.starts_with("#[rstest") {
                return true;
            }
            prev = p.prev_named_sibling();
        }
        false
    }

    fn walk(node: Node, text: &str, mods: &mut Vec<String>, f: &mut dyn FnMut(Node, &[String])) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            f(child, mods);
            if child.kind() == "mod_item"
                && let (Some(name), Some(body)) = (child.child_by_field_name("name"), child.child_by_field_name("body"))
            {
                mods.push(node_text(name, text).to_string());
                walk(body, text, mods, f);
                mods.pop();
            }
        }
    }

    let is_main_file = rel_str == "src/main.rs" || rel_str.starts_with("src/bin/") || rel_str.starts_with("examples/");
    walk(root, text, &mut Vec::new(), &mut |node, mods| {
        let Some(name) = node.child_by_field_name("name") else { return };
        let ident = node_text(name, text);
        let full: Vec<&str> = module.iter().chain(mods.iter()).map(String::as_str).chain([ident]).collect();
        match node.kind() {
            "function_item" if has_test_attr(node, text) => {
                let filter = full.join("::");
                out.push(target(
                    format!("Test '{ident}'"),
                    format!("cargo test{target_flag} -- {} --exact", sh_quote(&filter)),
                    &crate_dir,
                    node,
                    name,
                ));
            }
            "function_item" if ident == "main" && mods.is_empty() && is_main_file => {
                let run = if target_flag.contains("--test") { String::new() } else { target_flag.clone() };
                let label = if stem == "main" { "main" } else { stem.as_str() };
                out.push(target(format!("Run '{label}'"), format!("cargo run{run}"), &crate_dir, node, name));
            }
            "mod_item"
                if node.child_by_field_name("body").is_some()
                    && (node_text(node, text).contains("#[test") || has_cfg_test(node, text)) =>
            {
                let filter = full.join("::");
                out.push(target(format!("Tests in '{ident}'"), format!("cargo test{target_flag} -- {}", sh_quote(&filter)), &crate_dir, node, name));
            }
            _ => {}
        }
    });
}

fn has_cfg_test(node: Node, text: &str) -> bool {
    node.prev_named_sibling().is_some_and(|p| p.kind() == "attribute_item" && node_text(p, text).contains("cfg(test)"))
}

// ---- Go --------------------------------------------------------------------------

fn go_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let dir = path.parent().unwrap_or(path);
    let is_test_file = path.to_string_lossy().ends_with("_test.go");
    let is_main_package = text.lines().any(|l| l.trim() == "package main");
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor).filter(|n| n.kind() == "function_declaration") {
        let Some(name) = node.child_by_field_name("name") else { continue };
        let ident = node_text(name, text);
        if is_test_file && (ident.starts_with("Test") || ident.starts_with("Benchmark") || ident.starts_with("Example")) {
            let flag = if ident.starts_with("Benchmark") { "-bench" } else { "-run" };
            out.push(target(
                format!("Test '{ident}'"),
                format!("go test {flag} {} .", sh_quote(&format!("^{ident}$"))),
                dir,
                node,
                name,
            ));
        } else if ident == "main" && is_main_package {
            out.push(target("Run 'main'".into(), "go run .".into(), dir, node, name));
        }
    }
}

// ---- Python ----------------------------------------------------------------------

fn python_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let dir = path.parent().unwrap_or(path);
    let file = sh_quote(&path.to_string_lossy());
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        let node = if node.kind() == "decorated_definition" { node.child_by_field_name("definition").unwrap_or(node) } else { node };
        match node.kind() {
            "function_definition" => {
                let Some(name) = node.child_by_field_name("name") else { continue };
                let ident = node_text(name, text);
                if ident.starts_with("test") {
                    out.push(target(format!("Test '{ident}'"), format!("python3 -m pytest {file}::{ident}"), dir, node, name));
                }
            }
            "class_definition" => {
                let Some(name) = node.child_by_field_name("name") else { continue };
                let class = node_text(name, text);
                if !class.starts_with("Test") {
                    continue;
                }
                out.push(target(format!("Tests in '{class}'"), format!("python3 -m pytest {file}::{class}"), dir, node, name));
                let Some(body) = node.child_by_field_name("body") else { continue };
                let mut c = body.walk();
                for method in body.named_children(&mut c) {
                    let method = if method.kind() == "decorated_definition" {
                        method.child_by_field_name("definition").unwrap_or(method)
                    } else {
                        method
                    };
                    if method.kind() != "function_definition" {
                        continue;
                    }
                    let Some(mname) = method.child_by_field_name("name") else { continue };
                    let m = node_text(mname, text);
                    if m.starts_with("test") {
                        out.push(target(format!("Test '{m}'"), format!("python3 -m pytest {file}::{class}::{m}"), dir, method, mname));
                    }
                }
            }
            "if_statement" if node_text(node, text).starts_with("if __name__") => {
                out.push(target("Run file".into(), format!("python3 {file}"), dir, node, node));
            }
            _ => {}
        }
    }
}

// ---- JavaScript / TypeScript -------------------------------------------------------

/// The test runner a package uses.
fn js_test_runner(package_dir: &Path) -> &'static str {
    let package = std::fs::read_to_string(package_dir.join("package.json")).unwrap_or_default();
    if package.contains("\"vitest\"") {
        "vitest"
    } else if package.contains("\"jest\"") {
        "jest"
    } else {
        "node"
    }
}

fn js_targets(path: &Path, text: &str, root: Node, out: &mut Vec<RunTarget>) {
    let package_dir = nearest_with(path, "package.json").unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf());
    let runner = js_test_runner(&package_dir);
    let file = sh_quote(&path.to_string_lossy());
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
        if node.kind() != "call_expression" {
            continue;
        }
        let Some(function) = node.child_by_field_name("function") else { continue };
        // `test`, `it`, `describe` and their `.only` / `.skip` forms.
        let callee = node_text(function, text);
        let base = callee.split('.').next().unwrap_or(callee);
        if !matches!(base, "test" | "it" | "describe") {
            continue;
        }
        let Some(args) = node.child_by_field_name("arguments") else { continue };
        let Some(first) = args.named_child(0).filter(|a| matches!(a.kind(), "string" | "template_string")) else { continue };
        let title = node_text(first, text).trim_matches(['"', '\'', '`']).to_string();
        let pattern = regex::escape(&title);
        let command = match runner {
            "vitest" => format!("npx vitest run {file} -t {}", sh_quote(&pattern)),
            "jest" => format!("npx jest {file} -t {}", sh_quote(&pattern)),
            _ => format!("node --test --test-name-pattern={} {file}", sh_quote(&pattern)),
        };
        let label = if base == "describe" { format!("Tests in '{title}'") } else { format!("Test '{title}'") };
        out.push(target(label, command, &package_dir, node, node));
    }
}

// ---- Project configurations --------------------------------------------------------

/// What the project can run: build tools found at its root, then the
/// current file.
pub fn project_targets(root: &Path, current: Option<(&Path, Language)>) -> Vec<RunTarget> {
    let mut out = Vec::new();
    let mut add = |name: String, command: String, cwd: &Path| {
        out.push(RunTarget { name, command, cwd: cwd.to_path_buf(), row: None, rows: 0..0 });
    };
    if root.join("Cargo.toml").is_file() {
        add("cargo run".into(), "cargo run".into(), root);
        add("cargo test".into(), "cargo test".into(), root);
        add("cargo build".into(), "cargo build".into(), root);
        if let Ok(bins) = std::fs::read_dir(root.join("src/bin")) {
            for bin in bins.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "rs")) {
                let name = bin.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                add(format!("cargo run --bin {name}"), format!("cargo run --bin {}", sh_quote(&name)), root);
            }
        }
    }
    if let Ok(package) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&package)
        && let Some(scripts) = json.get("scripts").and_then(|s| s.as_object())
    {
        let manager = if root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
            "bun"
        } else {
            "npm"
        };
        for name in scripts.keys() {
            add(format!("{manager} run {name}"), format!("{manager} run {}", sh_quote(name)), root);
        }
    }
    if root.join("go.mod").is_file() {
        add("go run .".into(), "go run .".into(), root);
        add("go test ./...".into(), "go test ./...".into(), root);
        add("go build ./...".into(), "go build ./...".into(), root);
    }
    if let Ok(makefile) = std::fs::read_to_string(root.join("Makefile")) {
        for line in makefile.lines() {
            // `name:` rules, not variables (`A := b`) or special targets.
            let Some((target, rest)) = line.split_once(':') else { continue };
            if rest.starts_with('=') || target.starts_with(['.', '\t', ' ', '#']) || target.contains(['$', '%', '=']) {
                continue;
            }
            for t in target.split_whitespace() {
                add(format!("make {t}"), format!("make {}", sh_quote(t)), root);
            }
        }
    }
    if root.join("pyproject.toml").is_file() || root.join("pytest.ini").is_file() {
        add("pytest".into(), "python3 -m pytest".into(), root);
    }
    if let Some((path, language)) = current {
        let file = sh_quote(&path.to_string_lossy());
        let dir = path.parent().unwrap_or(root);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let command = match language {
            Language::Python => Some(format!("python3 {file}")),
            Language::JavaScript => Some(format!("node {file}")),
            Language::TypeScript | Language::Tsx => Some(format!("npx tsx {file}")),
            Language::Shell => Some(format!("bash {file}")),
            Language::Go => Some(format!("go run {file}")),
            Language::Php => Some(format!("php {file}")),
            _ => None,
        };
        if let Some(command) = command {
            add(format!("Run '{name}'"), command, dir);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn targets(language: Language, path: &Path, text: &str) -> Vec<(String, String, Option<usize>)> {
        let syntax = crate::syntax::Syntax::new(language, &Rope::from_str(text));
        file_targets(language, path, text, syntax.tree().unwrap())
            .into_iter()
            .map(|t| (t.name, t.command, t.row))
            .collect()
    }

    fn temp_project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kin-run-{}-{}", std::process::id(), files[0].0.replace('/', "_")));
        for (rel, content) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    #[test]
    fn rust_tests_and_main() {
        let src = "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() {}\n}\n";
        let dir = temp_project(&[("Cargo.toml", "[package]\nname = \"x\"\n"), ("src/main.rs", src)]);
        let got = targets(Language::Rust, &dir.join("src/main.rs"), src);
        assert_eq!(got[0], ("Run 'main'".into(), "cargo run".into(), Some(0)));
        assert_eq!(got[1], ("Tests in 'tests'".into(), "cargo test -- tests".into(), Some(3)));
        assert_eq!(got[2], ("Test 'adds'".into(), "cargo test -- tests::adds --exact".into(), Some(5)));
        let lib = "pub fn f() {}\n#[test]\nfn top() {}\n";
        let got = targets(Language::Rust, &dir.join("src/parser/mod.rs"), lib);
        assert_eq!(got, [("Test 'top'".into(), "cargo test -- parser::top --exact".into(), Some(2))]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn go_python_and_js() {
        let go = "package main\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {}\n";
        let got = targets(Language::Go, Path::new("/p/add_test.go"), go);
        assert_eq!(got, [("Test 'TestAdd'".into(), "go test -run '^TestAdd$' .".into(), Some(4))]);

        let py = "def test_one():\n    pass\n\nclass TestThing:\n    def test_two(self):\n        pass\n\nif __name__ == \"__main__\":\n    main()\n";
        let got = targets(Language::Python, Path::new("/p/t.py"), py);
        assert_eq!(got.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["Test 'test_one'", "Tests in 'TestThing'", "Test 'test_two'", "Run file"]);
        assert_eq!(got[2].1, "python3 -m pytest /p/t.py::TestThing::test_two");

        let dir = temp_project(&[("package.json", "{\"devDependencies\": {\"vitest\": \"1\"}}"), ("a.test.ts", "")]);
        let ts = "describe('math', () => {\n  it('adds 1+1', () => {});\n});\n";
        let got = targets(Language::TypeScript, &dir.join("a.test.ts"), ts);
        assert_eq!(got[0].0, "Tests in 'math'");
        assert!(got[1].1.starts_with("npx vitest run ") && got[1].1.ends_with("-t 'adds 1\\+1'"), "{}", got[1].1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn project_configurations() {
        let dir = temp_project(&[
            ("package.json", "{\"scripts\": {\"dev\": \"vite\", \"test\": \"vitest\"}}"),
            ("pnpm-lock.yaml", ""),
            ("Makefile", "VERSION := 1\n.PHONY: all\nall: build\nbuild:\n\tcc x.c\n"),
        ]);
        let names: Vec<String> = project_targets(&dir, None).into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["pnpm run dev", "pnpm run test", "make all", "make build"]);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(sh_quote("a b'c"), "'a b'\\''c'");
        assert_eq!(sh_quote("src/a.rs"), "src/a.rs");
    }
}
