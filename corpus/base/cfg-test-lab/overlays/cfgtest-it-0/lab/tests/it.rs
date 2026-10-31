use lab::helper;

#[test]
fn helper_from_outside() {
    assert_eq!(helper(), 42);
    let _ = missing_fn();
}
