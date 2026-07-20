use bonsai_acceptance_rust::total;

#[test]
fn totals_line_items() {
    assert_eq!(total(&[2, 3, 5]), 10);
}

#[test]
fn empty_order_has_zero_total() {
    assert_eq!(total(&[]), 0);
}
