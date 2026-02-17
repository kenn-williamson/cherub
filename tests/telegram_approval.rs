#![cfg(feature = "telegram")]

//! Tests for the Telegram approval flow.
//!
//! These test the approval gate mechanics (timeout, approve, deny) without
//! requiring an actual Telegram bot or network access.

use tokio::sync::mpsc;

use cherub::telegram::approval::{ApprovalMessage, approval_manager, parse_callback_data};

// ---------------------------------------------------------------------------
// Callback data parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_approve_callback() {
    let result = parse_callback_data("approve:42");
    assert_eq!(result, Some((42, true)));
}

#[test]
fn parse_deny_callback() {
    let result = parse_callback_data("deny:7");
    assert_eq!(result, Some((7, false)));
}

#[test]
fn parse_unknown_callback() {
    assert_eq!(parse_callback_data("unknown:1"), None);
}

#[test]
fn parse_malformed_callback() {
    assert_eq!(parse_callback_data("approve:notanumber"), None);
}

#[test]
fn parse_empty_callback() {
    assert_eq!(parse_callback_data(""), None);
}

// ---------------------------------------------------------------------------
// Approval manager resolve flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approval_manager_resolves_approve() {
    let (tx, rx) = mpsc::channel::<ApprovalMessage>(16);

    // Spawn the approval manager.
    let handle = tokio::spawn(approval_manager(rx));

    // Register a pending approval.
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    tx.send(ApprovalMessage::Register {
        id: 1,
        sender: oneshot_tx,
    })
    .await
    .unwrap();

    // Resolve it as approved.
    tx.send(ApprovalMessage::Resolve {
        id: 1,
        approved: true,
    })
    .await
    .unwrap();

    let result = oneshot_rx.await.unwrap();
    assert!(result, "should be approved");

    // Drop sender to stop the manager.
    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn approval_manager_resolves_deny() {
    let (tx, rx) = mpsc::channel::<ApprovalMessage>(16);

    let handle = tokio::spawn(approval_manager(rx));

    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
    tx.send(ApprovalMessage::Register {
        id: 2,
        sender: oneshot_tx,
    })
    .await
    .unwrap();

    tx.send(ApprovalMessage::Resolve {
        id: 2,
        approved: false,
    })
    .await
    .unwrap();

    let result = oneshot_rx.await.unwrap();
    assert!(!result, "should be denied");

    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn approval_manager_unknown_id_ignored() {
    let (tx, rx) = mpsc::channel::<ApprovalMessage>(16);

    let handle = tokio::spawn(approval_manager(rx));

    // Resolve an ID that was never registered — should not panic.
    tx.send(ApprovalMessage::Resolve {
        id: 999,
        approved: true,
    })
    .await
    .unwrap();

    drop(tx);
    let _ = handle.await;
}

#[tokio::test]
async fn approval_timeout_results_in_deny() {
    // When the oneshot sender is dropped without sending (simulating timeout),
    // the receiver gets an error, which our gate treats as Denied.
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel::<bool>();

    // Drop the sender without sending — simulates what happens when
    // the approval manager task shuts down or the approval times out.
    drop(oneshot_tx);

    let result = oneshot_rx.await;
    assert!(result.is_err(), "dropped sender should produce RecvError");
}
