#![allow(clippy::no_effect, unused)]
#![warn(clippy::needless_raw_string_hashes)]

fn main() {
    r#"\aaa"#;
    r##"\aaa"##;
    r##"Hello "world"!"##;
    r######" "### "## "# "######;
}
