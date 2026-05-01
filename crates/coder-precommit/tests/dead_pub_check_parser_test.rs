use coder_precommit::parse_added_pub_items;

#[test]
fn parses_simple_diff_pub_fn() {
    let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 0000000..1111111 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -0,0 +1,1 @@
+pub fn foo() {}
";
    let items = parse_added_pub_items(diff);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "fn");
    assert_eq!(items[0].name, "foo");
    assert_eq!(items[0].file, "src/lib.rs");
    assert_eq!(items[0].line, 1);
}

#[test]
fn parses_pub_struct_and_enum() {
    let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,0 +1,3 @@
+pub struct Foo;
+pub enum Bar { A }
+pub fn baz() {}
";
    let items = parse_added_pub_items(diff);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].kind, "struct");
    assert_eq!(items[0].name, "Foo");
    assert_eq!(items[1].kind, "enum");
    assert_eq!(items[1].name, "Bar");
    assert_eq!(items[2].kind, "fn");
    assert_eq!(items[2].name, "baz");
}

#[test]
fn tracks_file_and_line_correctly() {
    let diff = "\
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,0 +11,1 @@
+pub fn alpha() {}
--- a/src/b.rs
+++ b/src/b.rs
@@ -50,0 +51,2 @@
+pub fn beta() {}
+pub fn gamma() {}
";
    let items = parse_added_pub_items(diff);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].file, "src/a.rs");
    assert_eq!(items[0].line, 11);
    assert_eq!(items[0].name, "alpha");
    assert_eq!(items[1].file, "src/b.rs");
    assert_eq!(items[1].line, 51);
    assert_eq!(items[1].name, "beta");
    assert_eq!(items[2].file, "src/b.rs");
    assert_eq!(items[2].line, 52);
    assert_eq!(items[2].name, "gamma");
}

#[test]
fn v1_emits_pub_inside_test_module_lacking_semantic_awareness() {
    let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,0 +1,5 @@
+#[cfg(test)]
+mod tests {
+    pub fn helper() {}
+}
+
";
    let items = parse_added_pub_items(diff);
    assert_eq!(
        items.len(),
        1,
        "v1 has no semantic awareness — emits pub-in-test-module"
    );
    assert_eq!(items[0].name, "helper");
}

#[test]
fn extracts_zero_added_pub_items_from_empty_diff() {
    let items = parse_added_pub_items("");
    assert_eq!(items.len(), 0);
}

#[test]
fn ignores_pub_in_context_or_deletion_lines() {
    let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 pub fn pre_existing() {}
-pub fn old() {}
+pub fn new() {}
";
    let items = parse_added_pub_items(diff);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "new");
}

#[test]
fn parses_all_seven_kinds() {
    let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -0,0 +1,7 @@
+pub fn f() {}
+pub struct S;
+pub enum E {}
+pub trait T {}
+pub type Y = u32;
+pub const C: u32 = 0;
+pub static X: u32 = 0;
";
    let items = parse_added_pub_items(diff);
    assert_eq!(items.len(), 7);
    let kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["fn", "struct", "enum", "trait", "type", "const", "static"]
    );
}
