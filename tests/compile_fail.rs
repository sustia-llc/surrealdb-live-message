//! Compile-fail tests of the crate's public surface, run by `trybuild`.

/// Each program under `tests/ui/` fails to compile, with the diagnostics in
/// its `.stderr` file.
#[test]
fn ui() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
