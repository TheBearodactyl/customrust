#![feature(unix_sigpipe)]

#[unix_sigpipe = "sig_dfl"] //~ error: `unix_sigpipe` attribute can only be used on `fn main()`
struct S;

fn main() {}
