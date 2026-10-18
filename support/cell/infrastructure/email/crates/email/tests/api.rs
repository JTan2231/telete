use email::api::{Message, Receipt};

#[test]
fn message_and_receipt_use_the_owned_interface() -> Result<(), Box<dyn std::error::Error>> {
    let message: Message = serde_json::from_str(
        r#"{"subject":"subject","body":"line one\nline two","idempotency_key":"example/1"}"#,
    )?;
    assert_eq!(message.body, "line one\nline two");
    let receipt = Receipt {
        id: "message-1".into(),
    };
    assert_eq!(receipt.to_string(), "Accepted message-1");
    Ok(())
}
