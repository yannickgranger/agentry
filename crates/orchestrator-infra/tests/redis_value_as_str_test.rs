//! Exercises the three branches of [`redis_value_as_str`]: the two
//! handled `redis::Value` variants (BulkString / SimpleString) and the
//! catch-all `_ => None` arm via `Nil`.

use orchestrator_infra::redis_io::redis_value_as_str;

#[test]
fn bulk_string_decodes_to_some() {
    let v = redis::Value::BulkString(b"hello".to_vec());
    assert_eq!(redis_value_as_str(&v), Some("hello".to_string()));
}

#[test]
fn simple_string_clones_to_some() {
    let v = redis::Value::SimpleString("OK".to_string());
    assert_eq!(redis_value_as_str(&v), Some("OK".to_string()));
}

#[test]
fn nil_maps_to_none() {
    let v = redis::Value::Nil;
    assert_eq!(redis_value_as_str(&v), None);
}
