//@ check-pass

#![feature(min_exhaustive_patterns)]

enum Void {}
fn main() {
    let a: Option<Void> = None;
    let None = a;
}
