use std::env;
use std::fs;
use std::io::{self, Write};
use std::time::SystemTime;

fn main() {
    let expected = env::var("GT_TEST").expect("GT_TEST env missing");
    assert_eq!(expected, "ok", "unexpected env payload");

    let mut stdout = io::stdout();
    stdout.write_all(b"hello stdout\n").unwrap();
    let mut stderr = io::stderr();
    stderr.write_all(b"hello stderr\n").unwrap();

    let _now = SystemTime::now();

    let contents = fs::read_to_string("/data/hello.txt").expect("preopen read failed");
    assert!(contents.contains("wasi"), "preopen contents malformed");

    let mut random = [0u8; 4];
    getrandom::getrandom(&mut random).expect("random api failed");

    match fs::write("/data/blocked.txt", b"denied") {
        Ok(_) => panic!("write to read-only preopen succeeded"),
        Err(_) => {}
    }
}
