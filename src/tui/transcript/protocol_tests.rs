use super::*;

#[test]
fn neutral_projection_preserves_every_durable_field() {
    for entry in p::tests::entries() {
        let before = serde_json::to_value(&entry).unwrap();
        let display = TranscriptItem::from(entry);
        let after = serde_json::to_value(p::TranscriptItem::from(&display)).unwrap();
        assert_eq!(before, after);
        let json = after.to_string();
        assert!(!json.contains("Instant"));
        assert!(!json.contains("selected_tool"));
    }
}
